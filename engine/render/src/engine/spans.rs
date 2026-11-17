//! The `spans` feature (bevy/trace without a chrome file): every span's time summed in the process, by its name
//! ("system: sync_meshes", a render graph node...), and written per update at the end of a run (Engine::census).
//! Set as the process's subscriber first thing in main (Engine::spans_global). Needs RUST_LOG to let Bevy's info spans through, e.g. RUST_LOG=warn,bevy_ecs=info,bevy_render=info,bevy_app=info.
//! A chrome trace writes every span's start and end to a file, which made a traced frame run ~2x its time; this keeps
//! only a sum per name, so the ranks and most of the sizes hold untraced.

use bevy::log::tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};
use bevy::log::tracing::{field::Field, field::Visit, span, Subscriber};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// name -> (ns summed, times entered)
static SUMS: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);

struct Named(std::sync::Arc<str>, Option<Instant>);

struct NameOf(Option<String>);
impl Visit for NameOf {
    fn record_str(&mut self, f: &Field, v: &str) {
        if f.name() == "name" {
            self.0 = Some(v.to_string());
        }
    }
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        if f.name() == "name" {
            self.0 = Some(format!("{v:?}").trim_matches('"').to_string());
        }
    }
}

pub(crate) struct Sums;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Sums {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut n = NameOf(None);
        attrs.record(&mut n);
        let name = match n.0 {
            Some(x) => format!("{}: {x}", attrs.metadata().name()),
            None => attrs.metadata().name().to_string(),
        };
        if let Some(s) = ctx.span(id) {
            s.extensions_mut().insert(Named(name.into(), None));
        }
    }
    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(s) = ctx.span(id) {
            if let Some(n) = s.extensions_mut().get_mut::<Named>() {
                n.1 = Some(Instant::now());
            }
        }
    }
    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(s) = ctx.span(id) else { return };
        let mut ext = s.extensions_mut();
        let Some(n) = ext.get_mut::<Named>() else { return };
        let Some(t) = n.1.take() else { return };
        let ns = t.elapsed().as_nanos() as u64;
        let name = n.0.clone();
        drop(ext);
        let mut g = SUMS.lock().unwrap();
        let e = g.get_or_insert_with(HashMap::new).entry(name.to_string()).or_default();
        e.0 += ns;
        e.1 += 1;
    }
}

/// The process's tracing subscriber: the sums under RUST_LOG's filter, set before anything else sets one (a page's
/// App makes its LogPlugin before the 3D engine's, and the first global subscriber is the only one).
pub(crate) fn global() {
    use bevy::log::tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let sub = Registry::default().with(Sums).with(filter);
    if bevy::log::tracing::subscriber::set_global_default(sub).is_err() {
        eprintln!("raea spans: a tracing subscriber was already set; no spans summed");
    }
}

/// Forget what's summed (the warm-up's updates).
pub(crate) fn reset() {
    *SUMS.lock().unwrap() = None;
}

/// The top `k` spans by time, per update over `updates`.
pub(crate) fn print(updates: u64, k: usize) {
    let g = SUMS.lock().unwrap();
    let Some(m) = g.as_ref() else { return };
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let u = updates.max(1) as f64;
    let systems = m.iter().filter(|(k, _)| k.starts_with("system: ")).map(|(_, x)| x.1).sum::<u64>() as f64 / u;
    eprintln!("raea spans: over {updates} updates: {systems:.0} system runs an update, {} names", m.len());
    for (name, (ns, n)) in v.into_iter().take(k) {
        eprintln!("raea spans: {:8.3} ms {:7.1}x  {}", *ns as f64 / 1e6 / u, *n as f64 / u, &name[..name.len().min(140)]);
    }
}
