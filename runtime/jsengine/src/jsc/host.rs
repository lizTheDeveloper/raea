//! JsEngine on JavaScriptCore: a Context whose natives are closures (Context::set_native), promise jobs drained as the
//! outermost API call ends, ES modules through the system's loader (modules.rs), and the thread's run loop pumped
//! between frames for JavaScriptCore's GC timers.

#[cfg(target_vendor = "apple")]
use super::modules::Modules;
use super::{Context, pump};
use crate::{Arg, Fetch, JsEngine, Native};

pub struct Jsc {
    ctx: Context,
    /// the module loader, kept while the context runs modules (JavaScriptCore holds its delegate weakly)
    #[cfg(target_vendor = "apple")]
    modules: Option<Modules>,
}

impl Jsc {
    pub fn new() -> Jsc {
        Jsc { ctx: Context::new(), #[cfg(target_vendor = "apple")] modules: None }
    }
}

impl Default for Jsc {
    fn default() -> Self {
        Self::new()
    }
}

impl JsEngine for Jsc {
    fn name(&self) -> &'static str {
        "jsc"
    }

    fn native(&mut self, name: &str, f: Native) -> Result<(), String> {
        self.ctx.set_native(name, f);
        Ok(())
    }

    fn eval(&mut self, src: &str, file: &str) -> Result<(), String> {
        self.ctx.eval(src, file)
    }

    fn call(&mut self, func: &str, args: &[Arg]) -> Option<Result<f64, String>> {
        let args: Vec<super::Arg> = args
            .iter()
            .map(|a| match a {
                Arg::Num(n) => super::Arg::Num(*n),
                Arg::Str(s) => super::Arg::Str(s),
                Arg::Null => super::Arg::Null,
            })
            .collect();
        self.ctx.call_num(func, &args)
    }

    // ES modules come through the system's Objective-C loader (modules.rs): WebKitGTK's C API has none, so on Linux
    // a host that needs modules runs them on QuickJS (React's thread evaluates a classic script)
    #[cfg(not(target_vendor = "apple"))]
    fn module(&mut self, url: &str, _src: &str, _fetch: Fetch, _global: &str) -> Result<(), String> {
        Err(format!("{url}: ES modules on JavaScriptCore are Apple's only (its Objective-C loader); run this on QuickJS"))
    }

    #[cfg(target_vendor = "apple")]
    fn module(&mut self, url: &str, src: &str, fetch: Fetch, global: &str) -> Result<(), String> {
        let modules = self.modules.insert(Modules::new(self.ctx.raw(), fetch));
        let p = modules.eval(url, src)?;
        self.ctx.keep(global, p);
        Ok(())
    }

    fn time_limit(&mut self, limit: std::time::Duration) {
        self.ctx.time_limit(limit);
    }

    fn collect(&mut self) {
        self.ctx.collect();
    }

    fn collect_young(&mut self) -> bool {
        self.ctx.collect_young()
    }

    fn release_memory(&mut self) -> Option<f64> {
        super::release_free_memory()
    }

    fn idle(&mut self) {
        pump();
    }
}
