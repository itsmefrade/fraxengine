// SPDX-License-Identifier: LGPL-3.0-only
// Pull based exact decode: GOP parallel WASM workers (worker.js) feeding in-order frames to a consumer (the FEL compositor).
// Unlike pool.js, which only measures, frames are handed out with their planes. A bounded reorder buffer applies backpressure:
// workers on a GOP the consumer has not reached yet are paused while more than `budgetMB` of decoded frames wait.
import {loadFixtures} from './pool.js';

export class ExactStream {
  /** @param {{n:number, layers:string[], budgetMB?:number, startFrame?:number}} o  layers: 'bl' and/or 'el'; `startFrame` is the first
   *  picture to deliver (decoding starts at the GOP that contains it, default 0). `seek(frame)` repositions later without respawning workers. */
  constructor(o){
    this.n=o.n;this.layers=o.layers;this.budget=(o.budgetMB??1536)*1048576;
    this.fixtures=o.fixtures??{};   // {base, prefix}, default: web/fixtures/fel-p7
    this.startFrame=o.startFrame??0;this.gen=0;this.nextTask=0;this.minTask=0;this.seeks=0;
    this.stats={decoded:0,peakBufferedMB:0,peakBufferedFrames:0,wasmMB:0,workerInitMs:0,firstFrameMs:null};
  }
  async start(){
    const fx=this.fx=await loadFixtures(this.fixtures.base,this.fixtures.prefix);
    this.st={};for(const l of this.layers)this.st[l]={gops:fx.layerGops[l],curGop:0,curK:0,map:new Map(),waiter:null};
    this.buffered=0;this.bufFrames=0;this.failed=null;this.wasm=new Map();
    this.#position(this.startFrame);
    this.workers=[];
    const t=this.t0=performance.now();
    await new Promise((resolve,reject)=>{
      let left=this.n;
      for(let i=0;i<this.n;i++){
        const w=new Worker(new URL('./worker.js',import.meta.url),{type:'module'});
        w.run=null;w.task=null;
        w.onerror=e=>{this.#fail('worker error '+e.message);reject(new Error(this.failed));};
        w.onmessage=ev=>{if(ev.data.type==='ready'&&--left===0)resolve();};
        this.workers.push(w);
        w.postMessage({type:'init',id:i,wasmUrl:new URL('./wasm/aether_video_simd.wasm',import.meta.url).href});
      }
    });
    this.stats.workerInitMs=Math.round(performance.now()-t);
    for(const w of this.workers){
      w.onmessage=ev=>{
        const m=ev.data;
        if(m.type==='frame')this.#onFrame(m);
        else if(m.type==='aborted'||(m.type==='taskdone'&&w.aborting===m.taskId&&!m.error)){
          // acknowledgement of an abort (a task that finished before the abort arrived counts too): the worker is idle again
          if(w.aborting===m.taskId){w.aborting=null;w.task=null;this.#assign(w);this.#gates();this.#settleSeek();}
        }
        else if(m.type==='taskdone'){
          if(m.error){if(m.taskId<this.minTask)return;this.#fail(m.error);return;}
          {const exp=this.fx.layerGops[m.layer][m.gop].count;if(m.count!==exp)this.#fail(`gop ${m.gop} ${m.layer}: ${m.count} frames, expected ${exp}`);}
          this.wasm.set(m.id,m.wasmBytes);this.stats.wasmMB=Math.round([...this.wasm.values()].reduce((a,b)=>a+b,0)/1048576);
          this.#assign(w);this.#gates();
        }
      };
      this.#assign(w);
    }
    return this;
  }
  /** Rebuilds per layer state and the task queue so the stream delivers from `frame` (GOP containing it; frames before it inside that GOP are decoded and dropped). */
  #position(frame){
    const fx=this.fx;this.buffered=0;this.bufFrames=0;this.queue=[];
    for(const l of this.layers){
      const gops=fx.layerGops[l],s=this.st[l];
      let gi=0;for(let i=0;i<gops.length;i++){if(gops[i].first<=frame)gi=i;else break;}
      s.map.clear();s.curGop=gi;s.curK=Math.max(0,frame-gops[gi].first);
      for(let g=gi;g<gops.length;g++)this.queue.push({layer:l,gop:g,first:gops[g].first});
    }
    // tasks in stream order (the EL and BL GOPs do not share boundaries); ids only grow, so frames of an older generation are recognisable
    this.queue.sort((a,b)=>a.first-b.first||(a.layer<b.layer?-1:1));
    this.minTask=this.nextTask;this.queue.forEach(t=>{t.taskId=this.nextTask++;});
  }
  /**
   * Repositions to `frame` (global picture index of the fixture): in-flight GOP decodes are aborted (each worker finishes at most the
   * NAL it is decoding), buffered frames are dropped and the workers, already holding a compiled WASM instance, start on the GOPs of the new
   * position. Resolves when every worker is idle or on its new task; the next `next(layer)` then delivers `frame`.
   */
  seek(frame){
    if(this.failed)return Promise.reject(new Error(this.failed));
    const t0=performance.now();this.seeks++;
    this.#position(Math.max(0,Math.min(frame,this.fx.total-1)));
    for(const s of Object.values(this.st))s.waiter?.();
    const waiting=[];
    for(const w of this.workers){
      if(w.task&&w.aborting==null){w.aborting=w.task.taskId;w.postMessage({type:'abort',taskId:w.task.taskId});waiting.push(w);}
      else if(w.aborting!=null)waiting.push(w); // still cancelling from an earlier seek: it takes the new queue when it acknowledges
      else this.#assign(w);
    }
    this.#gates();
    return new Promise(resolve=>{this.#seekDone={left:waiting.length,t0,resolve};if(!waiting.length)this.#settleSeek();});
  }
  #seekDone=null;
  #settleSeek(){
    const d=this.#seekDone;if(!d)return;
    if(this.workers.some(w=>w.aborting!=null))return;
    this.#seekDone=null;this.stats.lastSeekAbortMs=Math.round((performance.now()-d.t0)*10)/10;d.resolve(this.stats.lastSeekAbortMs);
  }
  #fail(msg){this.failed=this.failed??msg;for(const s of Object.values(this.st))s.waiter?.();}
  #assign(w){
    if(w.aborting!=null)return; // waits for its abort acknowledgement
    const t=this.queue.shift();
    if(!t){w.task=null;return;}
    const g=this.fx.layerGops[t.layer][t.gop];
    const [off,len]=g.prefix??g.range;
    const bytes=this.fx[t.layer].slice(off,off+len).buffer;
    w.task=t;w.run=true;
    w.postMessage({type:'task',taskId:t.taskId,layer:t.layer,gop:t.gop,bytes,hash:false,skip:g.prefix?g.skip:0},[bytes]);
  }
  #gates(){
    for(const w of this.workers){
      if(!w.task||w.aborting!=null)continue;
      const flag=w.task.gop<=this.st[w.task.layer].curGop||this.buffered<this.budget;
      if(w.run!==flag){w.run=flag;w.postMessage({type:'run',taskId:w.task.taskId,run:flag});}
    }
  }
  #onFrame(m){
    if(m.taskId<this.minTask){return;} // frame of a task cancelled by seek()
    const s=this.st[m.layer];
    if(m.gop===s.curGop&&m.k<s.curK)return; // before the seek target inside its GOP
    let a=s.map.get(m.gop);if(!a){a=[];s.map.set(m.gop,a);}
    a[m.k]={Y:new Uint16Array(m.Y),U:new Uint16Array(m.U),V:new Uint16Array(m.V),width:m.w,height:m.h,poc:m.poc,bytes:m.bytes};
    this.buffered+=m.bytes;this.bufFrames++;this.stats.decoded++;
    this.stats.peakBufferedMB=Math.max(this.stats.peakBufferedMB,Math.round(this.buffered/1048576));
    this.stats.peakBufferedFrames=Math.max(this.stats.peakBufferedFrames,this.bufFrames);
    if(this.stats.firstFrameMs===null)this.stats.firstFrameMs=Math.round(performance.now()-this.t0);
    s.waiter?.();this.#gates();
  }
  /** Next frame of `layer` in display order as `{planes:{Y,U,V}, width, height, poc}`, or null after the last. */
  async next(layer){
    const s=this.st[layer];
    for(;;){
      if(this.failed)throw new Error(this.failed);
      if(s.curGop>=s.gops.length)return null;
      const a=s.map.get(s.curGop),f=a&&a[s.curK];
      if(f){
        a[s.curK]=null;this.buffered-=f.bytes;this.bufFrames--;s.curK++;
        if(s.curK>=s.gops[s.curGop].count){s.map.delete(s.curGop);s.curGop++;s.curK=0;}
        this.#gates();
        return {planes:{Y:f.Y,U:f.U,V:f.V},width:f.width,height:f.height,poc:f.poc};
      }
      await new Promise(r=>{s.waiter=r;});s.waiter=null;
    }
  }
  stop(){for(const w of this.workers||[])w.terminate();this.workers=[];}
}
