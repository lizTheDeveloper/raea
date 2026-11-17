//! V8 (rusty_v8, the `v8` crate; BSD-3 V8, MIT bindings): the fastest engine for a bake's React (Lane E, 2026-11-17:
//! the same Ch1 frames ran 248 ms each on JavaScriptCore's DFG and 35 ms on node's V8). Linux only for now; picked with
//! `BEVY_TSX_ENGINE=v8` (raea render --jit=v8, or RAEA_JIT=v8). Natives are Rust closures behind a V8 function whose data
//! is the closure; promise jobs run after every eval and call (an explicit microtask checkpoint), as the other engines
//! run theirs. Classic scripts only: ES modules stay on JavaScriptCore and QuickJS (js-exec's path, not a bake's).

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{Arg, Fetch, JsEngine, Native, Ret};

/// V8's platform, once a process (every isolate shares it). The JS threads run on 64 MB stacks (raea-runtime-react), so
/// V8's own stack limit is raised to fit React's deep commits.
fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        v8::V8::set_flags_from_string("--stack-size=32000");
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

pub struct V8 {
    // dropped before the isolate (fields drop in order)
    context: v8::Global<v8::Context>,
    isolate: v8::OwnedIsolate,
    /// each native's closure, owned here; V8 holds a pointer to it as its function's data
    natives: Vec<*mut Native>,
    /// time_limit's deadline (ms since `epoch`, 0: none), watched by a thread that terminates the script past it
    deadline: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    epoch: Instant,
    watching: bool,
}

impl V8 {
    pub fn new() -> Result<V8, String> {
        init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let context = {
            v8::scope!(let hs, &mut isolate);
            let c = v8::Context::new(hs, Default::default());
            v8::Global::new(hs, c)
        };
        Ok(V8 { context, isolate, natives: Vec::new(), deadline: Arc::new(AtomicU64::new(0)), alive: Arc::new(AtomicBool::new(true)), epoch: Instant::now(), watching: false })
    }

    /// Promise jobs, until none is left.
    fn jobs(&mut self) {
        self.isolate.perform_microtask_checkpoint();
    }
}

impl Drop for V8 {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        for p in self.natives.drain(..) {
            // SAFETY: made by Box::into_raw in `native`, freed once; no script runs past here
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

/// What a caught exception says: its message and, when it has one, its stack (as jsc.rs describes one).
fn describe(tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>) -> String {
    let Some(e) = tc.exception() else { return if tc.has_terminated() { "the script ran past its time limit".into() } else { "a script threw".into() } };
    let msg = e.to_string(tc).map(|s| s.to_rust_string_lossy(tc)).unwrap_or_else(|| "a script threw".into());
    match tc.stack_trace().and_then(|s| s.to_string(tc)).map(|s| s.to_rust_string_lossy(tc)) {
        Some(st) if !st.is_empty() && st != msg => format!("{msg}\n{st}"),
        _ => msg,
    }
}

/// A native's trampoline: its arguments as String(v), its Ret as a JS value, its Err thrown as an Error.
fn trampoline(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let Ok(ext) = v8::Local::<v8::External>::try_from(args.data()) else { return };
    // SAFETY: the data is the *mut Native `native` made, alive while the engine is
    let f = unsafe { &*(ext.value() as *const Native) };
    let a: Vec<String> = (0..args.length()).map(|i| args.get(i).to_string(scope).map(|s| s.to_rust_string_lossy(scope)).unwrap_or_default()).collect();
    match f(&a) {
        Ok(Ret::Undefined) => {}
        Ok(Ret::Num(n)) => rv.set(v8::Number::new(scope, n).into()),
        Ok(Ret::Str(s)) => match v8::String::new(scope, &s) {
            Some(js) => rv.set(js.into()),
            None => {
                let m = v8::String::new(scope, "a native's answer is too long for a JS string").unwrap();
                let e = v8::Exception::error(scope, m);
                scope.throw_exception(e);
            }
        },
        Err(e) => {
            let m = v8::String::new(scope, &e).unwrap_or_else(|| v8::String::empty(scope));
            let x = v8::Exception::error(scope, m);
            scope.throw_exception(x);
        }
    }
}

impl JsEngine for V8 {
    fn name(&self) -> &'static str {
        "v8"
    }

    fn native(&mut self, name: &str, f: Native) -> Result<(), String> {
        let ptr = Box::into_raw(Box::new(f));
        self.natives.push(ptr);
        v8::scope_with_context!(scope, &mut self.isolate, &self.context);
        let ext = v8::External::new(scope, ptr as *mut c_void);
        let func = v8::Function::builder(trampoline).data(ext.into()).build(scope).ok_or_else(|| format!("v8: native {name} wasn't made"))?;
        let key = v8::String::new(scope, name).ok_or("v8: a native's name")?;
        let global = scope.get_current_context().global(scope);
        global.set(scope, key.into(), func.into()).ok_or_else(|| format!("v8: native {name} wasn't set"))?;
        Ok(())
    }

    fn eval(&mut self, src: &str, file: &str) -> Result<(), String> {
        let r = {
            v8::scope_with_context!(scope, &mut self.isolate, &self.context);
            v8::tc_scope!(let tc, scope);
            let code = v8::String::new(tc, src).ok_or_else(|| format!("{file}: too long for a JS string"))?;
            let name = v8::String::new(tc, file).unwrap_or_else(|| v8::String::empty(tc));
            let origin = v8::ScriptOrigin::new(tc, name.into(), 0, 0, false, 0, None, false, false, false, None);
            match v8::Script::compile(tc, code, Some(&origin)).and_then(|s| s.run(tc)) {
                Some(_) => Ok(()),
                None => Err(format!("{file}: {}", describe(tc))),
            }
        };
        self.jobs();
        r
    }

    fn call(&mut self, func: &str, args: &[Arg]) -> Option<Result<f64, String>> {
        let r = {
            v8::scope_with_context!(scope, &mut self.isolate, &self.context);
            v8::tc_scope!(let tc, scope);
            let global = tc.get_current_context().global(tc);
            let key = v8::String::new(tc, func)?;
            let f = v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()?;
            let mut a: Vec<v8::Local<v8::Value>> = Vec::with_capacity(args.len());
            for x in args {
                a.push(match x {
                    Arg::Num(n) => v8::Number::new(tc, *n).into(),
                    Arg::Str(s) => match v8::String::new(tc, s) {
                        Some(s) => s.into(),
                        None => return Some(Err(format!("{func}: an argument is too long for a JS string"))),
                    },
                    Arg::Null => v8::null(tc).into(),
                });
            }
            let recv: v8::Local<v8::Value> = global.into();
            Some(match f.call(tc, recv, &a) {
                Some(v) => Ok(if v.is_number() { v.number_value(tc).unwrap_or(f64::NAN) } else { f64::NAN }),
                None => Err(describe(tc)),
            })
        };
        self.jobs();
        r
    }

    fn module(&mut self, url: &str, _src: &str, _fetch: Fetch, _global: &str) -> Result<(), String> {
        Err(format!("{url}: the V8 engine runs classic scripts only (ES modules: BEVY_TSX_ENGINE=jsc or quickjs)"))
    }

    fn time_limit(&mut self, limit: Duration) {
        let at = (self.epoch.elapsed() + limit).as_millis().max(1) as u64;
        self.deadline.store(at, Ordering::Relaxed);
        if self.watching {
            return;
        }
        self.watching = true;
        let (deadline, alive, epoch, handle) = (self.deadline.clone(), self.alive.clone(), self.epoch, self.isolate.thread_safe_handle());
        std::thread::Builder::new()
            .name("raea v8 watchdog".into())
            .spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    let d = deadline.load(Ordering::Relaxed);
                    if d > 0 && epoch.elapsed().as_millis() as u64 > d {
                        handle.terminate_execution();
                        deadline.store(0, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .ok();
    }

    fn collect(&mut self) {
        self.isolate.low_memory_notification();
    }
}
