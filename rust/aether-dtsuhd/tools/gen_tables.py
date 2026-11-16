import os
import re,sys
lines=open('/tmp/aether-dtsuhd-ref/ts103491_v121.txt',encoding='utf-8').read().split('\n')
def clean(ls):
    out=[]
    prev=''
    for l in ls:
        s=l.strip()
        if not s: continue
        if s=='ETSI' or s=='Syntax' or s.startswith('=====PAGE') or s.startswith('ETSI TS 103 491'):
            prev=s; continue
        if prev.startswith('=====PAGE') and re.fullmatch(r'\d+',s):
            prev=''; continue
        prev=s
        out.append(s)
    return out
# BETA table: lines 12955..14416 (1-based)
seg=clean(lines[12954:14416])
rows=[];cur=None
for s in seg:
    m=re.match(r'\{\s*//\s*quantization_level=\s*(\d+)',s)
    if m:
        cur=[int(m.group(1)),[]]; rows.append(cur); 
        rest=s[m.end():]
        continue
    if cur is None: continue
    s=s.replace('{','').replace('}',' ')
    for t in re.findall(r'-?\d+',s):
        cur[1].append(int(t))
print(len(rows), [ (r[0],len(r[1])) for r in rows[:3]], [(r[0],len(r[1])) for r in rows[-2:]])
bad=[(r[0],len(r[1])) for r in rows if len(r[1])!=r[0]//2+1]
print('bad',bad[:10])
# interpolation
i0=[i for i,l in enumerate(lines) if 'INTERPOLATION_FILTER_TABLE[512]' in l][0]
seg=clean(lines[i0+2:i0+600])
txt=' '.join(seg)
txt=txt.split('};')[0]
nums=re.findall(r'-?\d\.\d+e[-+]\d+',txt)
print('interp',len(nums))
open('/tmp/aether-dtsuhd-work/tables_extract.txt','w').write('')
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'src', 'tables_gen.rs'),'w') as f:
    f.write('//! Tables transcribed from ETSI TS 103 491 V1.2.1 clause 10.7.5 and 10.8.1 by Scripts/gen_tables.py.\n//! Generated, do not edit by hand.\n\n')
    f.write('/// BETA_DEQUANT_TABLE rows for quantization levels 2, 4, ..., 256 (row r holds level 2*(r+1), with level/2+1 entries).\n')
    f.write('pub static BETA_DEQUANT_ROWS: [&[i32]; %d] = [\n'%len(rows))
    for r in rows:
        f.write('    &[%s],\n'%', '.join(str(x) for x in r[1]))
    f.write('];\n\n')
    f.write('/// INTERPOLATION_FILTER_TABLE, 512 entries (first half of the symmetric 1024-tap LFE interpolation filter).\n')
    f.write('pub static INTERPOLATION_FILTER_HALF: [f64; %d] = [\n'%len(nums))
    for i in range(0,len(nums),4):
        f.write('    '+', '.join(nums[i:i+4])+',\n')
    f.write('];\n')

# ---- additional small tables ----
def grab_nums(start_marker, end_marker, pat=r'-?\d+'):
    i0=[i for i,l in enumerate(lines) if start_marker in l][0]
    seg=clean(lines[i0+1:i0+80])
    txt=' '.join(seg)
    txt=txt.split('{',1)[1].split(end_marker,1)[0]
    return txt
mb=grab_nums('const uint MAX_BITS_FOR_N[','};')
mbn=[int(x) for x in re.findall(r'\d+',mb)]
mk=grab_nums('const uint MAX_K_FOR_N[','};')
mkn=[int(x) for x in re.findall(r'\d+',mk)]
i0=[i for i,l in enumerate(lines) if 'const uint K_ESTIMATION_COEFFICIENTS' in l][0]
i1=[i for i,l in enumerate(lines) if i>i0 and l.rstrip().endswith('}};')][0]
txt=' '.join(clean(lines[i0+1:i1+1]))
pairs=[(int(a),int(b)) for a,b in re.findall(r'\{\s*(\d+)\s*,\s*(\d+)\s*\}',txt)]
print('maxbits',len(mbn),'maxk',len(mkn),'kest',len(pairs))
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'src', 'tables_gen.rs'),'a') as f:
    f.write('\n/// MAX_BITS_FOR_N, first entry is for N=2.\npub static MAX_BITS_FOR_N: [i32; %d] = [\n'%len(mbn))
    for i in range(0,len(mbn),16): f.write('    '+', '.join(map(str,mbn[i:i+16]))+',\n')
    f.write('];\n\n/// MAX_K_FOR_N, first entry is for N=1.\npub static MAX_K_FOR_N: [i32; %d] = [\n'%len(mkn))
    for i in range(0,len(mkn),16): f.write('    '+', '.join(map(str,mkn[i:i+16]))+',\n')
    f.write('];\n\n/// K_ESTIMATION_COEFFICIENTS as (c0, c1), first entry is for N=2.\npub static K_ESTIMATION_COEFFICIENTS: [(i32, i32); %d] = [\n'%len(pairs))
    for i in range(0,len(pairs),6): f.write('    '+', '.join('(%d, %d)'%p for p in pairs[i:i+6])+',\n')
    f.write('];\n')
