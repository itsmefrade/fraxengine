// SPDX-License-Identifier: LGPL-3.0-only
// Main-side scheduler, display-order reassembly, bounded reorder buffer, metrics.
const FPS=24000/1001;
const fixtureCaches=new Map();let refsCache={};
/** Loads `<prefix>-index.json`, `<prefix>-bl.hevc`, `<prefix>-el.hevc` (Annex B) from `base`. Default: the 1454 frame clip. */
export async function loadFixtures(base=new URL('../fixtures/',import.meta.url),prefix='fel-p7'){
  const key=String(base)+prefix;
  if(fixtureCaches.has(key))return fixtureCaches.get(key);
  const [index,bl,el]=await Promise.all([
    fetch(new URL(prefix+'-index.json',base)).then(r=>r.json()),
    fetch(new URL(prefix+'-bl.hevc',base)).then(r=>r.arrayBuffer()),
    fetch(new URL(prefix+'-el.hevc',base)).then(r=>r.arrayBuffer())]);
  const aus=index.aus,gops=[];
  for(let i=0;i<aus.length;i++){
    if(aus[i].key||!gops.length)gops.push({first:i,count:0});
    gops[gops.length-1].count++;
  }
  for(const g of gops){
    const a=aus[g.first],b=aus[g.first+g.count-1];
    g.bl=[a.bl[0],b.bl[0]+b.bl[1]-a.bl[0]];g.el=[a.el[0],b.el[0]+b.el[1]-a.el[0]];
  }
  // Per layer GOPs. The EL has its own IRAP positions (index flag `elKey`, falls back to `key`). An open GOP (CRA with RASL pictures,
  // flags `rasl` / `elRasl`) needs the previous GOP as decode prefix: `prefix` is the byte range to decode and `skip` the number of
  // output frames of that prefix to drop (its own RASL pictures are not output when the prefix starts at a CRA).
  const layerGops={};
  for(const [l,keyF,raslF] of [['bl','key','rasl'],['el','elKey','elRasl']]){
    const gl=[];
    for(let i=0;i<aus.length;i++){
      const k=aus[i][keyF]??aus[i].key;
      if(k||!gl.length)gl.push({first:i,count:0,rasl:0});
      const g=gl[gl.length-1];g.count++;if(aus[i][raslF])g.rasl++;
    }
    for(let j=0;j<gl.length;j++){
      const g=gl[j],a=aus[g.first],b=aus[g.first+g.count-1];
      g.range=[a[l][0],b[l][0]+b[l][1]-a[l][0]];
      if(j>0&&g.rasl>0){const p=gl[j-1];g.prefix=[p.range[0],g.range[0]+g.range[1]-p.range[0]];g.skip=p.count-p.rasl;}
    }
    layerGops[l]=gl;
  }
  const fx={bl:new Uint8Array(bl),el:new Uint8Array(el),gops,layerGops,total:aus.length,display:index.display??null};
  fixtureCaches.set(key,fx);
  return fx;
}
export async function loadRefs(layer){
  if(!refsCache[layer])refsCache[layer]=await fetch(new URL(`./refs/${layer}.json`,import.meta.url)).then(r=>r.json());
  return refsCache[layer];
}

/** opts: {n, layers:['bl','el'], gopStart, gopCount, hash, budgetMB, pace, prebuffer, mem} */
export async function run(opts){
  const fx=await loadFixtures();
  const n=opts.n,layers=opts.layers,gopStart=opts.gopStart??0;
  const gopEnd=Math.min(fx.gops.length,gopStart+(opts.gopCount??n*2));
  const budget=(opts.budgetMB??1536)*1048576,hash=!!opts.hash;
  const refs={};if(hash)for(const l of layers)refs[l]=await loadRefs(l);
  const tClick=performance.now();
  const tasks=[];let tid=0;
  for(let g=gopStart;g<gopEnd;g++)for(const l of layers)tasks.push({taskId:tid++,layer:l,gop:g});
  const totalFrames=fx.gops.slice(gopStart,gopEnd).reduce((s,g)=>s+g.count,0);
  // reorder state
  const buf={};for(const l of layers)buf[l]=new Map(); // gop -> [frame|undefined]
  let curGop=gopStart,curK=0,consumed=0,buffered=0,peakBuf=0,peakFrames=0,bufFrames=0;
  const availTs=[],arrivalTs={};for(const l of layers)arrivalTs[l]=[];
  let tFirst=0,tLast=0,mismatches=0,firstMismatch=null,verified=0,orderViol=0,countErr=0,stalls=0,stallMs=0;
  let playStart=null,paceTimer=null;
  const wasmSizes=new Map(),taskInfo=[];
  let resolveDone,failed=null;const done=new Promise(r=>{resolveDone=r;});
  const workers=[];let readyMs=[];
  const queue=tasks.slice();
  const active=new Map(); // worker -> task

  const ready=await new Promise((resolve,reject)=>{
    let left=n;const t=performance.now();
    for(let i=0;i<n;i++){
      const w=new Worker(new URL('./worker.js',import.meta.url),{type:'module'});
      w.run=null;w.onerror=e=>{failed='worker error '+e.message;resolveDone();};
      w.onmessage=ev=>{const m=ev.data;if(m.type==='ready'){readyMs.push(m.ms);if(--left===0)resolve(performance.now()-t);}};
      workers.push(w);
      w.postMessage({type:'init',id:i,wasmUrl:new URL('./wasm/aether_video_simd.wasm',import.meta.url).href});
    }
  });
  const t0=performance.now();

  function setRun(w,flag){if(w.run!==flag){w.run=flag;w.postMessage({type:'run',taskId:w.task.taskId,run:flag});}}
  function gates(){
    for(const w of workers){
      if(!w.task)continue;
      setRun(w,w.task.gop<=curGop||buffered<budget);
    }
  }
  function assign(w){
    const t=queue.shift();
    if(!t){w.task=null;return;}
    const g=fx.gops[t.gop],[off,len]=g[t.layer];
    const bytes=fx[t.layer].slice(off,off+len).buffer;
    w.task=t;w.run=true;
    w.postMessage({type:'task',taskId:t.taskId,layer:t.layer,gop:t.gop,bytes,hash},[bytes]);
  }
  function hasFrame(l){const a=buf[l].get(curGop);return a&&a[curK];}
  function consume(){
    const now=performance.now();
    for(;;){
      if(curGop>=gopEnd)break;
      if(!layers.every(hasFrame)){
        if(playStart!==null&&opts.pace&&!layers.every(hasFrame)&&consumed>0&&!buf._stalled){buf._stalled=now;stalls++;}
        break;
      }
      if(opts.pace){
        if(playStart===null){
          if(consumed===0&&contiguousReady()<(opts.prebuffer??24)&&queue.length+[...workers].filter(w=>w.task).length>0)break;
          playStart=now;
        }
        const due=playStart+consumed/FPS;
        if(now<due){clearTimeout(paceTimer);paceTimer=setTimeout(()=>{consume();gates();},due-now);break;}
      }
      if(buf._stalled){const d=now-buf._stalled;stallMs+=d;if(opts.pace)playStart+=d;buf._stalled=0;}
      for(const l of layers){
        const a=buf[l].get(curGop),f=a[curK];
        buffered-=f.bytes;bufFrames--;a[curK]=null;
      }
      availTs.push(now);if(consumed===0)tFirst=now;tLast=now;consumed++;curK++;
      if(curK>=fx.gops[curGop].count){for(const l of layers)buf[l].delete(curGop);curGop++;curK=0;}
    }
    if(consumed===totalFrames)resolveDone();
  }
  function contiguousReady(){
    let c=0,g=curGop,k=curK;
    while(g<gopEnd){
      const cnt=fx.gops[g].count;
      for(;k<cnt;k++){if(!layers.every(l=>{const a=buf[l].get(g);return a&&a[k];}))return c;c++;}
      g++;k=0;
    }
    return c;
  }
  function onFrame(m,w){
    const now=performance.now();
    const gopInfo=fx.gops[m.gop],global=gopInfo.first+m.k;
    arrivalTs[m.layer][global-fx.gops[gopStart].first]=now;
    if(hash){
      const r=refs[m.layer].frames[global];verified++;
      if(!r||r[0]!==m.hashes[0]||r[1]!==m.hashes[1]||r[2]!==m.hashes[2]){mismatches++;if(!firstMismatch)firstMismatch={layer:m.layer,global,poc:m.poc,plane:r?['Y','U','V'].filter((_, i)=>r[i]!==m.hashes[i]).join(''):'noref'};}
      m.bytes=0;
    }
    let a=buf[m.layer].get(m.gop);if(!a){a=[];buf[m.layer].set(m.gop,a);}
    a[m.k]={bytes:m.bytes,Y:m.Y,U:m.U,V:m.V};
    buffered+=m.bytes;bufFrames++;if(buffered>peakBuf)peakBuf=buffered;if(bufFrames>peakFrames)peakFrames=bufFrames;
    consume();gates();
  }
  for(const w of workers){
    w.onmessage=ev=>{
      const m=ev.data;
      if(m.type==='frame')onFrame(m,w);
      else if(m.type==='taskdone'){
        if(m.error){failed=m.error;resolveDone();return;}
        taskInfo.push({layer:m.layer,gop:m.gop,count:m.count,wallMs:m.wallMs,w:m.w,h:m.h});
        if(!m.orderOk)orderViol++;
        if(m.count!==fx.gops[m.gop].count)countErr++;
        wasmSizes.set(m.id,m.wasmBytes);
        assign(w);gates();
      }
    };
    assign(w);
  }
  // safety timeout
  const timer=setTimeout(()=>{failed='timeout';resolveDone();},opts.timeoutMs??170000);
  await done;clearTimeout(timer);clearTimeout(paceTimer);
  let uaMem=null;
  if(opts.mem&&self.crossOriginIsolated&&performance.measureUserAgentSpecificMemory){
    try{const r=await performance.measureUserAgentSpecificMemory();uaMem=Math.round(r.bytes/1048576);}catch(e){uaMem='err '+e.message;}
  }
  for(const w of workers)w.terminate();
  // minimum startup delay for glitch-free realtime playout of this run (from run start), in ms
  let need=0;for(let i=0;i<availTs.length;i++)need=Math.max(need,availTs[i]-t0-i*1000/FPS);
  const wasmMB=Math.round([...wasmSizes.values()].reduce((a,b)=>a+b,0)/1048576);
  const span=(tLast-tFirst)/1000;
  return {n,layers:layers.join('+'),hash,pace:!!opts.pace,gops:[gopStart,gopEnd],frames:consumed,expected:totalFrames,failed,
    workerInitMs:Math.round(ready),
    firstFrameMs:Math.round(tFirst-t0),firstFrameFromClickMs:Math.round(tFirst-tClick),
    totalMs:Math.round(tLast-t0),
    overallFps:+(consumed/((tLast-t0)/1000)).toFixed(2),
    sustainedFps:consumed>1?+((consumed-1)/span).toFixed(2):null,
    realtimeX:consumed>1?+(((consumed-1)/span)/FPS).toFixed(2):null,
    minStartupForRealtimeMs:Math.round(need),
    peakBufferedFrames:peakFrames,peakBufferedMB:Math.round(peakBuf/1048576),wasmMB,uaMemMB:uaMem,
    verified,mismatches,firstMismatch,orderViol,countErr,stalls,stallMs:Math.round(stallMs),
    tasks:taskInfo.length};
}
