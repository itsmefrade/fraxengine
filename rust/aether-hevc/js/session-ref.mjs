/** Persistent one-layer raw-NAL decoder (aether-hevc). Dedicated Worker usage only in browser; Node supported.
 * Same contract as web/exact/session.mjs, but pictures are read straight out of the decoder's picture buffers:
 * exactly one copy of each plane (into a fresh Uint16Array the caller owns) instead of three. */
const MAX_INPUT=8*1024*1024;
const CODES={1:'unknown-session',2:'epoch',3:'finished',4:'poisoned',5:'backpressure',6:'invalid-nal',7:'decode-error',8:'epoch-must-advance'};
function integer(x,name){if(!Number.isInteger(x)||x<0||x>0xffffffff)throw new RangeError(name+' must be u32');return x;}
export async function loadAetherVideoSessions(url=new URL('./wasm/aether_hevc_simd.wasm',import.meta.url)){
  if(typeof window!=='undefined'&&window.document)throw new Error('Persistent decoder must run in a DedicatedWorker');
  const response=await fetch(url);if(!response.ok)throw new Error('HEVC WASM HTTP'+response.status);
  const {instance}=await WebAssembly.instantiate(await response.arrayBuffer(),{}),wasm=instance.exports;let trapped=false;
  if(!wasm.aether_hevc_session_new_ref)throw new Error('WASM module lacks the reference-output API');
  const live=()=>{if(trapped)throw new Error('WASM trapped; discard Worker and instance');};
  const call=fn=>{live();try{return fn();}catch(e){if(e instanceof WebAssembly.RuntimeError)trapped=true;throw e;}};
  /** Reads a result block: throws for a non-zero status, else returns the payload offset and count; frees the block via done(). */
  function open(pointer){
    if(!pointer)throw new Error('Missing session result');
    const view=new DataView(wasm.memory.buffer),size=view.getUint32(pointer,true);
    if(size<12||pointer+size>wasm.memory.buffer.byteLength){trapped=true;throw new Error('Invalid session result bounds; discard instance');}
    const status=view.getUint32(pointer+4,true),count=view.getUint32(pointer+8,true);
    const free=()=>{if(!trapped)call(()=>wasm.aether_video_free(pointer,size));};
    if(status){const error=new Error(new TextDecoder().decode(new Uint8Array(wasm.memory.buffer,pointer+12,size-12)));error.code=CODES[status]??'unknown';free();throw error;}
    return {view,size,count,free,pointer};
  }
  function takePlane(ptr,stride,w,rows){
    if(w<1||rows<1||ptr%2!==0||ptr+((rows-1)*stride+w)*2>wasm.memory.buffer.byteLength)throw new Error('Invalid plane bounds');
    const mem=wasm.memory.buffer;
    if(stride===w)return new Uint16Array(mem,ptr,w*rows).slice();
    const out=new Uint16Array(w*rows);
    for(let r=0;r<rows;r++)out.set(new Uint16Array(mem,ptr+r*stride*2,w),r*w);
    return out;
  }
  return {
    get memoryBytes(){return wasm.memory.buffer.byteLength;},
    /** Decode plan for a task that starts with its predecessor GOP as prefix (open GOP). Returns {keep:Uint8Array (1 = push this NAL), skip} or
     * null when nothing can be pruned. The planner reads only parameter sets and slice headers (heads of the NALs are copied in). */
    plan(nals){
      if(!wasm.aether_hevc_plan||nals.length===0)return null;
      const heads=nals.map(n=>{const t=(n[0]>>1)&63;return n.subarray(0,t<32?Math.min(n.length,1024):(t>=32&&t<=34)||t===36||t===37?n.length:Math.min(n.length,2));});
      let total=4+4*heads.length;for(const h of heads)total+=h.length;
      if(total>MAX_INPUT)return null;
      return call(()=>{
        const ptr=wasm.aether_video_alloc(total);if(!ptr)return null;
        let r;
        try{
          const m=new Uint8Array(wasm.memory.buffer,ptr,total),dv=new DataView(wasm.memory.buffer,ptr,total);
          dv.setUint32(0,heads.length,true);let o=4+4*heads.length;
          for(let i=0;i<heads.length;i++){dv.setUint32(4+4*i,heads[i].length,true);m.set(heads[i],o);o+=heads[i].length;}
          r=open(call(()=>wasm.aether_hevc_plan(ptr,total)));
        }finally{if(!trapped)call(()=>wasm.aether_video_free(ptr,total));}
        try{
          if(r.count!==nals.length||!r.view.getUint32(r.pointer+12,true))return null;
          const skip=r.view.getUint32(r.pointer+16,true);
          return {keep:new Uint8Array(wasm.memory.buffer,r.pointer+20,r.count).slice(),skip};
        }finally{r.free();}
      });
    },
    createSession(epoch=0){
      integer(epoch,'epoch');const handle=call(()=>wasm.aether_hevc_session_new_ref(epoch));if(!handle)throw new Error('At most two active sessions per instance');let closed=false,currentEpoch=epoch;
      const active=()=>{live();if(closed)throw new Error('Session closed');};
      const simple=pointer=>{const r=open(pointer);r.free();};
      return {
        get epoch(){return currentEpoch;},get wasmMemoryBytes(){return wasm.memory.buffer.byteLength;},
        pushNal(bytes,epoch=currentEpoch){active();integer(epoch,'epoch');if(!(bytes instanceof Uint8Array)||bytes.length===0||bytes.length>MAX_INPUT)throw new RangeError('Complete1..8MiB raw NAL required');return call(()=>{const ptr=wasm.aether_video_alloc(bytes.length);if(!ptr)throw new Error('NAL allocation rejected');try{new Uint8Array(wasm.memory.buffer,ptr,bytes.length).set(bytes);simple(call(()=>wasm.aether_video_session_push(handle,ptr,bytes.length,epoch)));}finally{if(!trapped)call(()=>wasm.aether_video_free(ptr,bytes.length));}});},
        pull(epoch=currentEpoch,discard=false){active();integer(epoch,'epoch');return call(()=>{
          const r=open(wasm.aether_hevc_session_next_ref(handle,epoch));
          try{
            if(r.count===0)return null;
            if(r.count!==1||r.size!==12+17*4)throw new Error('Unexpected session result');
            const u=Array.from({length:17},(_,j)=>r.view.getUint32(r.pointer+12+j*4,true));
            const [width,height,bitDepth,chromaFormat,poc]=u;
            if(discard)return {width,height,bitDepth,chromaFormat,poc:poc|0,Y:null,U:null,V:null,epoch};   // picture dropped by the caller: no copy
            if(width<1||height<1||width*height>3840*2160)throw new Error('Invalid session dimensions');
            const Y=takePlane(u[5],u[6],u[7],u[8]),U=takePlane(u[9],u[10],u[11],u[12]),V=takePlane(u[13],u[14],u[15],u[16]);
            return {width,height,bitDepth,chromaFormat,poc:poc|0,Y,U,V,epoch};
          }finally{r.free();}
        });},
        finish(epoch=currentEpoch){active();integer(epoch,'epoch');call(()=>simple(wasm.aether_video_session_finish(handle,epoch)));},
        reset(epoch){active();integer(epoch,'epoch');call(()=>simple(wasm.aether_video_session_reset(handle,epoch)));currentEpoch=epoch;},
        close(){if(closed)return;closed=true;if(!trapped)call(()=>{if(wasm.aether_video_session_close(handle))throw new Error('Unknown session handle');});},
      };
    },
  };
}
