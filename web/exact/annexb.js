// SPDX-License-Identifier: LGPL-3.0-only
// Annex B splitting. Returns raw NALs (no start code, trailing zero bytes trimmed).
export function splitAnnexB(bytes){
  const n=bytes.length,starts=[];
  for(let i=0;i+2<n;){
    if(bytes[i+2]>1){i+=3;continue;}
    if(bytes[i]===0&&bytes[i+1]===0&&bytes[i+2]===1){starts.push(i+3);i+=3;}else i++;
  }
  const out=[];
  for(let s=0;s<starts.length;s++){
    let end=s+1<starts.length?starts[s+1]-3:n;
    while(end>starts[s]&&bytes[end-1]===0)end--;
    if(end>starts[s])out.push(bytes.subarray(starts[s],end));
  }
  return out;
}
