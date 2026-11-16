// SPDX-License-Identifier: LGPL-3.0-only
/** Persistent one-layer raw-NAL decoder. Dedicated Worker usage only in browser. */
const MAX_INPUT=8*1024*1024;
const LITTLE_ENDIAN=new Uint8Array(new Uint16Array([1]).buffer)[0]===1;
const CODES={1:'unknown-session',2:'epoch',3:'finished',4:'poisoned',5:'backpressure',6:'invalid-nal',7:'decode-error',8:'epoch-must-advance'};
function integer(x,name){if(!Number.isInteger(x)||x<0||x>0xffffffff)throw new RangeError(name+' must be u32');return x;}
export async function loadAetherVideoSessions(url=new URL('./aether_video_simd.wasm',import.meta.url)){
  if(typeof window!=='undefined'&&window.document)throw new Error('Persistent decoder must run in a DedicatedWorker');
  const response=await fetch(url);if(!response.ok)throw new Error('HEVC WASM HTTP'+response.status);
  const {instance}=await WebAssembly.instantiate(await response.arrayBuffer(),{}),wasm=instance.exports;let trapped=false;
  const live=()=>{if(trapped)throw new Error('WASM trapped; discard Worker and instance');};
  function result(pointer,maxFrames){
    let size=0;
    try{
      if(!pointer)throw new Error('Missing session result');const view=new DataView(wasm.memory.buffer);size=view.getUint32(pointer,true);
      if(size<12||pointer+size>wasm.memory.buffer.byteLength){trapped=true;throw new Error('Invalid session result bounds; discard instance');}const status=view.getUint32(pointer+4,true),count=view.getUint32(pointer+8,true);
      if(status){const error=new Error(new TextDecoder().decode(new Uint8Array(wasm.memory.buffer,pointer+12,size-12)));error.code=CODES[status]??'unknown';throw error;}
      if(count>maxFrames)throw new Error('Session frame count exceeded');const frames=[];let cursor=pointer+12;
      for(let i=0;i<count;i++){
        if(cursor+32>pointer+size)throw new Error('Truncated session frame');const h=Array.from({length:8},(_,j)=>view.getUint32(cursor+j*4,true));cursor+=32;
        const [width,height,bitDepth,chromaFormat,poc,yCount,uCount,vCount]=h;const total=yCount+uCount+vCount;
        if(width<1||height<1||width*height>3840*2160||cursor+total*2>pointer+size)throw new Error('Invalid session dimensions');
        const copy=n=>{let plane;if(LITTLE_ENDIAN&&cursor%2===0)plane=new Uint16Array(wasm.memory.buffer,cursor,n).slice();else{plane=new Uint16Array(n);for(let k=0;k<n;k++)plane[k]=view.getUint16(cursor+2*k,true);}cursor+=n*2;return plane;};
        frames.push({width,height,bitDepth,chromaFormat,poc:poc|0,Y:copy(yCount),U:copy(uCount),V:copy(vCount)});
      }
      if(cursor!==pointer+size)throw new Error('Session result trailing bytes');return frames;
    }finally{if(pointer&&size&&!trapped)call(()=>wasm.aether_video_free(pointer,size));}
  }
  const call=fn=>{live();try{return fn();}catch(e){if(e instanceof WebAssembly.RuntimeError)trapped=true;throw e;}};
  return {
    createSession(epoch=0){
      integer(epoch,'epoch');const handle=call(()=>wasm.aether_video_session_new(epoch));if(!handle)throw new Error('At most two active sessions per instance');let closed=false,currentEpoch=epoch;
      const active=()=>{live();if(closed)throw new Error('Session closed');};
      return {
        get epoch(){return currentEpoch;},get wasmMemoryBytes(){return wasm.memory.buffer.byteLength;},
        pushNal(bytes,epoch=currentEpoch){active();integer(epoch,'epoch');if(!(bytes instanceof Uint8Array)||bytes.length===0||bytes.length>MAX_INPUT)throw new RangeError('Complete1..8MiB raw NAL required');return call(()=>{const ptr=wasm.aether_video_alloc(bytes.length);if(!ptr)throw new Error('NAL allocation rejected');try{new Uint8Array(wasm.memory.buffer,ptr,bytes.length).set(bytes);result(call(()=>wasm.aether_video_session_push(handle,ptr,bytes.length,epoch)),0);}finally{if(!trapped)call(()=>wasm.aether_video_free(ptr,bytes.length));}});},
        pull(epoch=currentEpoch){active();integer(epoch,'epoch');return call(()=>{const frame=result(wasm.aether_video_session_next(handle,epoch),1)[0];return frame?{...frame,epoch}:null;});},
        finish(epoch=currentEpoch){active();integer(epoch,'epoch');call(()=>result(wasm.aether_video_session_finish(handle,epoch),0));},
        reset(epoch){active();integer(epoch,'epoch');call(()=>result(wasm.aether_video_session_reset(handle,epoch),0));currentEpoch=epoch;},
        close(){if(closed)return;closed=true;if(!trapped)call(()=>{if(wasm.aether_video_session_close(handle))throw new Error('Unknown session handle');});},
      };
    },
  };
}
