import struct,sys,json
f=open(sys.argv[1],'rb'); data=f.read()
def boxes(buf,start,end):
    p=start
    while p+8<=end:
        sz,typ=struct.unpack('>I4s',buf[p:p+8]); hdr=8
        if sz==1: sz=struct.unpack('>Q',buf[p+8:p+16])[0]; hdr=16
        if sz==0: sz=end-p
        yield typ.decode('latin1'),p+hdr,p+sz
        p+=sz
def find(buf,s,e,path):
    for t,a,b in boxes(buf,s,e):
        if t==path[0]:
            if len(path)==1: return a,b
            r=find(buf,a,b,path[1:])
            if r: return r
    return None
moov=find(data,0,len(data),['moov'])
for t,a,b in boxes(data,*moov):
    if t!='trak': continue
    mdia=find(data,a,b,['mdia'])
    hd=find(data,*mdia,['hdlr']); 
    if data[hd[0]+8:hd[0]+12]!=b'soun': continue
    stbl=find(data,*find(data,*mdia,['minf']),['stbl'])
    stsd=find(data,*stbl,['stsd'])
    print('stsd raw',data[stsd[0]:stsd[1]].hex()[:400])
    # sample entry
    e=stsd[0]+8
    sz,typ=struct.unpack('>I4s',data[e:e+8]); print('entry',typ,sz)
    # audio sample entry: 8 hdr +6 reserved+2 dri + 8 + 2 ch +2 ss + 4 + 4 srate = 36 bytes
    for t2,a2,b2 in boxes(data,e+36,e+sz):
        print(' child',t2,b2-a2,data[a2:b2].hex())
    stsz=find(data,*stbl,['stsz']); stsc=find(data,*stbl,['stsc']); stco=find(data,*stbl,['stco']); co64=find(data,*stbl,['co64'])
    ss,cnt=struct.unpack('>II',data[stsz[0]+4:stsz[0]+12])
    sizes=[ss]*cnt if ss else list(struct.unpack('>%dI'%cnt,data[stsz[0]+12:stsz[0]+12+4*cnt]))
    n=struct.unpack('>I',data[stsc[0]+4:stsc[0]+8])[0]
    stscs=[struct.unpack('>III',data[stsc[0]+8+12*i:stsc[0]+20+12*i]) for i in range(n)]
    if stco:
        m=struct.unpack('>I',data[stco[0]+4:stco[0]+8])[0]; offs=list(struct.unpack('>%dI'%m,data[stco[0]+8:stco[0]+8+4*m]))
    else:
        m=struct.unpack('>I',data[co64[0]+4:co64[0]+8])[0]; offs=list(struct.unpack('>%dQ'%m,data[co64[0]+8:co64[0]+8+8*m]))
    stts=find(data,*stbl,['stts']); k=struct.unpack('>I',data[stts[0]+4:stts[0]+8])[0]
    print('stts',[struct.unpack('>II',data[stts[0]+8+8*i:stts[0]+16+8*i]) for i in range(k)])
    mdhd=find(data,*mdia,['mdhd']); print('mdhd ts',struct.unpack('>I',data[mdhd[0]+12:mdhd[0]+16]))
    out=open(sys.argv[2],'wb'); si=0; pk=[]
    for ci,off in enumerate(offs):
        spc=[x for x in stscs if x[0]<=ci+1][-1][1]
        o=off
        for _ in range(spc):
            if si>=cnt: break
            out.write(data[o:o+sizes[si]]); pk.append(sizes[si]); o+=sizes[si]; si+=1
    json.dump(pk,open(sys.argv[2]+'.idx','w'))
    print('packets',len(pk),'min/max',min(pk),max(pk))
