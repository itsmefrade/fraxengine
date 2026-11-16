// SPDX-License-Identifier: LGPL-3.0-only
// One decode worker: own WASM instance, one reusable session. Decodes whole GOPs (IDR to next IDR).
import {loadAetherVideoSessions} from './session.mjs';
import {splitAnnexB} from './annexb.js';

let api=null,session=null,epoch=0,id=-1;
const running=new Map(); // taskId -> boolean (flow control from main)
const aborted=new Set(); // taskIds main asked to cancel (unique per run, so a late abort for a finished task is harmless)
const mc=new MessageChannel();let yieldResolve=null;
mc.port1.onmessage=()=>{const r=yieldResolve;yieldResolve=null;if(r)r();};
const yieldMacro=()=>new Promise(r=>{yieldResolve=r;mc.port2.postMessage(0);});
let wake=null;
const hex=b=>{const u=new Uint8Array(b);let s='';for(let i=0;i<u.length;i++)s+=u[i].toString(16).padStart(2,'0');return s;};

self.onmessage=async e=>{
  const m=e.data;
  if(m.type==='init'){
    id=m.id;const t=performance.now();
    api=await loadAetherVideoSessions(m.wasmUrl);
    postMessage({type:'ready',id,ms:performance.now()-t});
  }else if(m.type==='task'){
    running.set(m.taskId,true);
    runTask(m).catch(err=>postMessage({type:'taskdone',id,taskId:m.taskId,error:String(err&&err.stack||err)}));
  }else if(m.type==='abort'){
    // Cancels the task in flight (the worker and its WASM session stay for the next task). A decode call already running
    // finishes first (one NAL), then the task posts {type:'aborted'} instead of 'taskdone'; frames of the task already posted
    // are stale and carry its taskId.
    if(running.has(m.taskId)){aborted.add(m.taskId);running.set(m.taskId,true);if(wake){const w=wake;wake=null;w();}}
  }else if(m.type==='run'){
    if(running.has(m.taskId)&&!aborted.has(m.taskId)){running.set(m.taskId,m.run);if(m.run&&wake){const w=wake;wake=null;w();}}
  }
};

async function runTask(t){
  const t0=performance.now();
  const nals=splitAnnexB(new Uint8Array(t.bytes));
  if(!session){session=api.createSession(0);epoch=0;}
  else{epoch++;session.reset(epoch);}
  let skipLeft=t.skip||0;let count=0,lastPoc=-Infinity,orderOk=true,bytesOut=0,w=0,h=0,decodeMs=0;
  const isAborted=()=>aborted.has(t.taskId);
  const finishAborted=()=>{running.delete(t.taskId);aborted.delete(t.taskId);postMessage({type:'aborted',id,taskId:t.taskId,gop:t.gop,layer:t.layer});};
  async function emit(f){
    if(skipLeft>0){skipLeft--;return;}
    if(isAborted())return;
    const k=count++;if(f.poc<=lastPoc)orderOk=false;lastPoc=f.poc;w=f.width;h=f.height;
    const msg={type:'frame',id,taskId:t.taskId,layer:t.layer,gop:t.gop,k,poc:f.poc,w,h,bytes:(f.Y.length+f.U.length+f.V.length)*2};
    if(t.hash){
      const d=await Promise.all([f.Y,f.U,f.V].map(p=>crypto.subtle.digest('SHA-256',p)));
      msg.hashes=d.map(hex);postMessage(msg);
    }else{
      msg.Y=f.Y.buffer;msg.U=f.U.buffer;msg.V=f.V.buffer;
      postMessage(msg,[msg.Y,msg.U,msg.V]);
    }
    bytesOut+=msg.bytes;
  }
  async function drain(){let f;while(!isAborted()&&(f=session.pull())){await emit(f);}}
  for(const nal of nals){
    while(!running.get(t.taskId)){await new Promise(r=>{wake=r;});}
    await yieldMacro(); // lets 'run' and 'abort' messages in
    if(isAborted())return finishAborted();
    const a=performance.now();
    for(;;){
      try{session.pushNal(nal);break;}
      catch(err){if(err.code==='backpressure'){await drain();continue;}throw err;}
    }
    decodeMs+=performance.now()-a;
    const b=performance.now();await drain();decodeMs+=0; // drain time includes copy-out; counted in wall only
    if(isAborted())return finishAborted();
  }
  session.finish();await drain();
  if(isAborted())return finishAborted();
  running.delete(t.taskId);
  postMessage({type:'taskdone',id,taskId:t.taskId,layer:t.layer,gop:t.gop,count,orderOk,w,h,
    wallMs:performance.now()-t0,wasmBytes:session.wasmMemoryBytes});
}
