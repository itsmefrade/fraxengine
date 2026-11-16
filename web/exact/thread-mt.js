// SPDX-License-Identifier: LGPL-3.0-only
// One host thread of the threaded aether-hevc decoder (session-mt.mjs): its own instance of the module on the shared memory,
// its own stack, then aether_thread_main, which runs decoder tasks until the worker is terminated.
import {futexImports} from './session-mt.mjs';
self.onmessage = e => {
  const {module, memory, index, stackTop} = e.data;
  const {exports} = new WebAssembly.Instance(module, {env: {memory, ...futexImports(memory)}});
  exports.__stack_pointer.value = stackTop;   // before any Rust code runs: the default stack is the decode thread's
  postMessage({type: 'up', index});
  exports.aether_thread_main(index);          // never returns
};
