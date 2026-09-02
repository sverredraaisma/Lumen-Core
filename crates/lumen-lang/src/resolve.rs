//! Name resolution, type checking, and the section analysis that makes hoisting
//! pay.
//!
//! # Sections
//!
//! Every expression is placed in the cheapest section that can compute it:
//!
//! - **`once`** — depends on nothing but constants.
//! - **`frame`** — depends on time or channels, but not on which LED this is.
//! - **`pixel`** — depends on position, index, or the history buffer.
//!
//! Placement is derived, never declared: an expression's section is the maximum
//! over what it reads. A `let` that mentions only `t` lands in `frame` and is
//! computed once instead of three hundred times, and the author never has to
//! think about it.
//!
//! When a `let` *cannot* be hoisted, that is a warning with the reason attached
//! — knowing which input dragged an expression into the per-pixel path is the
//! difference between an author fixing it in a minute and never noticing.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::*;
use crate::diag::{Diagnostic, Diagnostics, Span};

/// Which section an expression can be computed in.
///
/// Ordered: `Once < Frame < Pixel`, and an expression's section is the maximum
/// over its inputs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Rate {
    Once,
    Frame,
    Pixel,
}

/// A built-in read-only variable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builtin {
    pub name: &'static str,
    pub ty: Type,
    pub rate: Rate,
    /// Set when referencing it requires a capability.
    pub requires: Option<Cap>,
}

/// Every built-in, with the section it forces and the capability it needs.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "t",
        ty: Type::Float,
        rate: Rate::Frame,
        requires: None,
    },
    // `dt` is pixel-invariant, so it hoists automatically even though the
    // grammar allows reading it per pixel.
    Builtin {
        name: "dt",
        ty: Type::Float,
        rate: Rate::Frame,
        requires: None,
    },
    Builtin {
        name: "n",
        ty: Type::Int,
        rate: Rate::Frame,
        requires: None,
    },
    Builtin {
        name: "mapq",
        ty: Type::Int,
        rate: Rate::Frame,
        requires: None,
    },
    Builtin {
        name: "x",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "y",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "z",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "pos",
        ty: Type::Vec3,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "lx",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "ly",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "lz",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "i",
        ty: Type::Int,
        rate: Rate::Pixel,
        requires: None,
    },
    Builtin {
        name: "u",
        ty: Type::Float,
        rate: Rate::Pixel,
        requires: None,
    },
    // Referencing `uv` without `requires grid` is an error, not a warning: the
    // value would be meaningless on a device with no grid projection.
    Builtin {
        name: "uv",
        ty: Type::Vec2,
        rate: Rate::Pixel,
        requires: Some(Cap::Grid),
    },
    Builtin {
        name: "prev",
        ty: Type::Color,
        rate: Rate::Pixel,
        requires: None,
    },
];

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// Signature of a core stdlib function.
///
/// The core set is frozen in the VM, so this table is the compiler's half of
/// that contract. Growth happens in the versioned source library instead.
pub struct Signature {
    pub name: &'static str,
    /// Accepted argument counts. Several entries means the function is
    /// overloaded on arity, like `noise`.
    pub arity: &'static [usize],
    pub ret: Type,
}

/// The frozen core: one instruction each, or a short inline sequence.
pub const CORE_FNS: &[Signature] = &[
    Signature {
        name: "abs",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "floor",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "fract",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "sqrt",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "sin",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "cos",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "sin01",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "cos01",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "exp",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "log",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "pow",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "atan2",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "min",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "max",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "clamp",
        arity: &[3],
        ret: Type::Float,
    },
    Signature {
        name: "step",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "smoothstep",
        arity: &[3],
        ret: Type::Float,
    },
    Signature {
        name: "mix",
        arity: &[3],
        ret: Type::Float,
    },
    Signature {
        name: "select",
        arity: &[3],
        ret: Type::Float,
    },
    Signature {
        name: "noise1",
        arity: &[1],
        ret: Type::Float,
    },
    Signature {
        name: "noise2",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "noise3",
        arity: &[3],
        ret: Type::Float,
    },
    Signature {
        name: "length",
        arity: &[2, 3],
        ret: Type::Float,
    },
    Signature {
        name: "dot",
        arity: &[2],
        ret: Type::Float,
    },
    Signature {
        name: "vec2",
        arity: &[2],
        ret: Type::Vec2,
    },
    Signature {
        name: "vec3",
        arity: &[3],
        ret: Type::Vec3,
    },
    Signature {
        name: "rgb",
        arity: &[3],
        ret: Type::Color,
    },
    Signature {
        name: "hsv",
        arity: &[3],
        ret: Type::Color,
    },
    Signature {
        name: "temp",
        arity: &[2],
        ret: Type::Color,
    },
    Signature {
        name: "palette",
        arity: &[2],
        ret: Type::Color,
    },
];

pub fn core_fn(name: &str) -> Option<&'static Signature> {
    CORE_FNS.iter().find(|s| s.name == name)
}

/// What a name refers to.
#[derive(Clone, PartialEq, Debug)]
pub enum SymbolKind {
    Param,
    /// The per-pixel history buffer.
    State,
    Channel,
    Let,
    Mask,
    Palette,
    Curve,
    Fn,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub ty: Type,
    pub rate: Rate,
    pub span: Span,
    /// Index into the corresponding list on the effect.
    pub index: usize,
}

/// A `let` after analysis, with the section it belongs in.
#[derive(Clone, PartialEq, Debug)]
pub struct ResolvedLet {
    pub name: String,
    pub ty: Type,
    pub rate: Rate,
    pub index: usize,
    /// Whether anything reads it. An unread binding is left out of the program
    /// entirely: it would otherwise hold one of the VM's 32 registers for the
    /// whole frame while contributing nothing.
    pub used: bool,
}

/// An effect that type-checks.
#[derive(Clone, PartialEq, Debug)]
pub struct Resolved<'a> {
    pub effect: &'a Effect,
    pub palettes: Vec<&'a Palette>,
    pub symbols: BTreeMap<String, Symbol>,
    /// `let` bindings in declaration order, each tagged with its section.
    pub lets: Vec<ResolvedLet>,
    pub stdlib: crate::StdlibVersion,
}

/// Resolve and type-check the first effect in a file.
///
/// Palettes and functions declared alongside it travel with it — a file is
/// self-contained, so everything the effect needs is either here or in the
/// vendored stdlib.
pub fn resolve<'a>(file: &'a File, diags: &mut Diagnostics) -> Option<Resolved<'a>> {
    if file.language_version != crate::LANGUAGE_VERSION {
        diags.push(Diagnostic::error(
            file.span,
            alloc::format!(
                "this compiler implements language version {}, but the file declares {}",
                crate::LANGUAGE_VERSION,
                file.language_version
            ),
            "update the compiler, or change the `lumen` header if the file is newer than it needs to be",
        ));
        return None;
    }

    let effect = file.decls.iter().find_map(|d| match d {
        Decl::Effect(e) => Some(e),
        _ => None,
    });
    let effect = match effect {
        Some(e) => e,
        None => {
            diags.push(Diagnostic::error(
                file.span,
                "no `effect` declaration in this file",
                "a compilable file contains at least one `effect \"name\" { ... }`",
            ));
            return None;
        }
    };

    let palettes: Vec<&Palette> = file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Palette(p) => Some(p),
            _ => None,
        })
        .collect();

    let mut r = Resolver {
        diags,
        symbols: BTreeMap::new(),
        has_grid: effect.requires.contains(&Cap::Grid),
        read: alloc::collections::BTreeSet::new(),
        used: alloc::collections::BTreeSet::new(),
        fns: effect.fns.clone(),
    };

    for (index, p) in palettes.iter().enumerate() {
        r.declare(
            &p.name,
            Symbol {
                kind: SymbolKind::Palette,
                ty: Type::Palette,
                rate: Rate::Once,
                span: p.span,
                index,
            },
        );
    }
    for (index, c) in file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Curve(c) => Some(c),
            _ => None,
        })
        .enumerate()
    {
        r.declare(
            &c.name,
            Symbol {
                kind: SymbolKind::Curve,
                ty: Type::Curve,
                rate: Rate::Once,
                span: c.span,
                index,
            },
        );
    }
    for (index, f) in effect.fns.iter().enumerate() {
        r.declare(
            &f.name,
            Symbol {
                kind: SymbolKind::Fn,
                ty: f.ret.unwrap_or(Type::Float),
                rate: Rate::Once,
                span: f.span,
                index,
            },
        );
    }

    // A function that calls itself, directly or through another, cannot be
    // inlined - and inlining is the only thing the compiler does with them.
    // Catching it here names a function on the cycle; catching it in the
    // emitter would only report that nesting got too deep.
    for f in &effect.fns {
        if let Some(via) = calls_itself(f, &effect.fns) {
            r.diags.push(Diagnostic::error(
                f.span,
                alloc::format!("function `{}` is recursive (through `{via}`)", f.name),
                "functions are always inlined, so they cannot recurse - rewrite it without the cycle",
            ));
        }
    }

    // Parameters are constants at compile time: they change between activations,
    // never within one.
    for (index, p) in effect.params.iter().enumerate() {
        if p.ty == Type::Float && p.range.is_none() {
            r.diags.push(Diagnostic::error(
                p.span,
                alloc::format!("float parameter `{}` has no range", p.name),
                "add `range lo..hi`; without bounds the parameter cannot be shown as a slider or bound to a MIDI control",
            ));
        }
        if let (Some(unit), ExprKind::Number { unit: lit_unit, .. }) = (p.unit, &p.default.kind) {
            if lit_unit.is_none() {
                r.diags.push(Diagnostic::warning(
                    p.default.span,
                    alloc::format!(
                        "default for `{}` has no unit, but the parameter declares `{}`",
                        p.name,
                        unit.as_str()
                    ),
                    alloc::format!("write the default as `{}{}`", "…", unit.as_str()),
                ));
            }
        }
        r.declare(
            &p.name,
            Symbol {
                kind: SymbolKind::Param,
                ty: p.ty,
                rate: Rate::Once,
                span: p.span,
                index,
            },
        );
    }

    // A channel forces `frame` at best: its value changes between frames but is
    // the same for every pixel.
    for (index, c) in effect.channels.iter().enumerate() {
        let ty = match &c.ty {
            ChanType::Vec3 => Type::Vec3,
            _ => Type::Float,
        };
        r.declare(
            &c.name,
            Symbol {
                kind: SymbolKind::Channel,
                ty,
                rate: Rate::Frame,
                span: c.span,
                index,
            },
        );
    }

    // States are readable everywhere a `let` is, at pixel rate: the value is
    // this pixel's colour from the previous frame.
    for (index, st) in effect.states.iter().enumerate() {
        r.declare(
            &st.name,
            Symbol {
                kind: SymbolKind::State,
                ty: Type::Color,
                rate: Rate::Pixel,
                span: st.span,
                index,
            },
        );
    }

    // `let` and `mask` in declaration order, each seeing what came before.
    let mut lets: Vec<ResolvedLet> = Vec::new();
    for (index, b) in effect.lets.iter().enumerate() {
        let (ty, rate) = r.check(&b.value);
        r.declare(
            &b.name,
            Symbol {
                kind: SymbolKind::Let,
                ty,
                rate,
                span: b.span,
                index,
            },
        );
        lets.push(ResolvedLet {
            name: b.name.clone(),
            ty,
            rate,
            index,
            // Filled in below, once everything that could read it has been
            // checked.
            used: false,
        });
        if rate == Rate::Pixel {
            if let Some(reason) = r.why_pixel(&b.value) {
                r.diags.push(Diagnostic::warning(
                    b.span,
                    alloc::format!("`{}` is computed per pixel because it reads `{reason}`", b.name),
                    "if this does not need to vary per LED, rewrite it so it does not read that value - work in `frame` is done once instead of once per LED",
                ));
            }
        }
    }
    for (index, m) in effect.masks.iter().enumerate() {
        let (_, rate) = r.check(&m.value);
        r.declare(
            &m.name,
            Symbol {
                kind: SymbolKind::Mask,
                ty: Type::Bool,
                rate,
                span: m.span,
                index,
            },
        );
    }

    // Layers.
    if effect.layers.is_empty() {
        r.diags.push(Diagnostic::error(
            effect.span,
            "effect has no layers",
            "add a `layer base { color = ... }`; an effect that assigns no colour renders nothing",
        ));
    }
    for layer in &effect.layers {
        if let Some(mask) = &layer.mask {
            match r.symbols.get(mask) {
                Some(s) if s.kind == SymbolKind::Mask => {}
                Some(_) => r.diags.push(Diagnostic::error(
                    layer.span,
                    alloc::format!("`{mask}` is not a mask"),
                    "a layer mask must name something declared with `mask`",
                )),
                None => r.diags.push(Diagnostic::error(
                    layer.span,
                    alloc::format!("unknown mask `{mask}`"),
                    "declare it with `mask name = <expression>` before the layer",
                )),
            }
        }
        let mut local: Vec<String> = Vec::new();
        for b in &layer.lets {
            let (ty, rate) = r.check(&b.value);
            r.declare(
                &b.name,
                Symbol {
                    kind: SymbolKind::Let,
                    ty,
                    rate,
                    span: b.span,
                    index: usize::MAX,
                },
            );
            local.push(b.name.clone());
        }
        if !layer.assigns.iter().any(|a| a.target == "color") {
            r.diags.push(Diagnostic::error(
                layer.span,
                alloc::format!("layer `{}` never assigns `color`", layer.name),
                "every layer must assign `color`; that is what it contributes to the pixel",
            ));
        }
        for a in &layer.assigns {
            let (ty, _) = r.check(&a.value);
            if a.target == "color" && a.field.is_none() && !matches!(ty, Type::Color | Type::Vec3) {
                r.diags.push(Diagnostic::error(
                    a.span,
                    alloc::format!("`color` must be a colour, but this is `{}`", ty.as_str()),
                    "use `rgb(r, g, b)`, `hsv(h, s, v)` or a palette lookup",
                ));
            }
        }
        // Layer-local bindings go out of scope with the layer.
        for name in local {
            r.symbols.remove(&name);
        }
        if let Some(op) = &layer.opacity {
            r.check(op);
        }
    }

    // Constructs the compiler parses but cannot yet emit. Refusing loudly beats
    // compiling something that silently does less than the author wrote.
    // `state` maps onto the VM's per-pixel history buffer, of which there is
    // exactly one. More than one declaration cannot be honoured, and quietly
    // aliasing them would produce an effect that looks nearly right and is not.
    if effect.states.len() > 1 {
        for extra in &effect.states[1..] {
            r.diags.push(Diagnostic::error(
                extra.span,
                "an effect may declare only one `state`",
                "the device keeps one history buffer per pixel; combine the values into a single `color` state",
            ));
        }
    }
    for st in &effect.states {
        if st.ty != Type::Color {
            r.diags.push(Diagnostic::error(
                st.span,
                alloc::format!("`state` must be a `color`, not `{}`", st.ty.as_str()),
                "the per-pixel history buffer holds a colour; use `rgb(...)` for the initial value",
            ));
        }
        let (_, _) = r.check(&st.init);
    }
    for s in &effect.sims {
        r.diags.push(Diagnostic::error(
            s.span,
            "`sim` blocks are not implemented yet",
            "remove the sim, or render the effect without it for now",
        ));
    }

    // Usage is only known once masks, layers and later bindings have all been
    // checked, so this is settled here rather than at the declaration.
    for l in &mut lets {
        l.used = r.used.contains(&l.name);
    }
    for l in &lets {
        if !l.used {
            let span = effect.lets[l.index].span;
            r.diags.push(Diagnostic::warning(
                span,
                alloc::format!("`{}` is never read", l.name),
                "remove it - registers are the scarce resource on this VM, and a hoisted binding holds one for the whole frame",
            ));
        }
    }

    // A channel declared but never read is almost always a leftover.
    for c in &effect.channels {
        if !r.read.contains(&c.name) {
            r.diags.push(Diagnostic::warning(
                c.span,
                alloc::format!("channel `{}` is declared but never read", c.name),
                "remove it, or read it - a declared channel makes the effect amber and costs bandwidth",
            ));
        }
    }

    let stdlib = effect
        .stdlib
        .map(|v| crate::StdlibVersion(v as u16))
        .unwrap_or(crate::DEFAULT_STDLIB);
    if !crate::stdlib::has(stdlib) {
        let known: Vec<String> = crate::stdlib::available()
            .iter()
            .map(|v| alloc::format!("{}", v.0))
            .collect();
        r.diags.push(Diagnostic::error(
            effect.span,
            alloc::format!("this compiler does not have stdlib version {}", stdlib.0),
            alloc::format!(
                "it carries {}; update the compiler, or lower the `stdlib` line",
                known.join(", ")
            ),
        ));
    }

    if diags_has_errors(r.diags) {
        return None;
    }

    Some(Resolved {
        effect,
        palettes,
        symbols: r.symbols,
        lets,
        stdlib,
    })
}

fn diags_has_errors(d: &Diagnostics) -> bool {
    d.has_errors()
}

struct Resolver<'d> {
    diags: &'d mut Diagnostics,
    symbols: BTreeMap<String, Symbol>,
    has_grid: bool,
    /// Channels actually read, so an unread one can be reported.
    read: alloc::collections::BTreeSet<String>,
    /// Every name an expression referenced, so a binding nobody reads can be
    /// left out of the program rather than holding a register for the whole
    /// frame. Registers, not instructions, are the binding constraint on this
    /// VM - 32 of them, and a hoisted binding holds one until the frame ends.
    used: alloc::collections::BTreeSet<String>,
    /// The effect's functions, for arity checking at the call site.
    fns: Vec<FnDecl>,
}

impl Resolver<'_> {
    fn declare(&mut self, name: &str, sym: Symbol) {
        if let Some(prev) = self.symbols.get(name) {
            // A stdlib declaration carries an empty span, because its real
            // position is in a file the author cannot see.
            let help = if prev.span == Span::EMPTY {
                alloc::format!("`{name}` is already in the standard library; pick another name")
            } else {
                alloc::format!("the earlier declaration is at byte {}", prev.span.start)
            };
            self.diags.push(Diagnostic::error(
                sym.span,
                alloc::format!("`{name}` is already declared"),
                help,
            ));
            return;
        }
        self.symbols.insert(name.to_string(), sym);
    }

    /// Type and section of an expression.
    fn check(&mut self, e: &Expr) -> (Type, Rate) {
        match &e.kind {
            ExprKind::Number { .. } => (Type::Float, Rate::Once),
            ExprKind::Color(_) => (Type::Color, Rate::Once),
            ExprKind::Str(_) => (Type::Float, Rate::Once),
            ExprKind::Ident(name) => {
                if let Some(b) = builtin(name) {
                    if let Some(cap) = b.requires {
                        if cap == Cap::Grid && !self.has_grid {
                            self.diags.push(Diagnostic::error(
                                e.span,
                                alloc::format!("`{name}` needs a grid projection"),
                                "add `requires grid` to the effect; without it `uv` has no meaning on a device",
                            ));
                        }
                    }
                    return (b.ty, b.rate);
                }
                match self.symbols.get(name) {
                    Some(s) => {
                        let out = (s.ty, s.rate);
                        if s.kind == SymbolKind::Channel {
                            self.read.insert(name.clone());
                        }
                        self.used.insert(name.clone());
                        out
                    }
                    None => {
                        self.diags.push(Diagnostic::error(
                            e.span,
                            alloc::format!("unknown name `{name}`"),
                            "declare it with `param`, `channel` or `let`, or check the spelling",
                        ));
                        (Type::Float, Rate::Once)
                    }
                }
            }
            ExprKind::Field { base, field } => {
                let (ty, rate) = self.check(base);
                let ok = matches!(
                    (&ty, field.as_str()),
                    (Type::Vec2, "x" | "y" | "u" | "v")
                        | (Type::Vec3, "x" | "y" | "z")
                        | (Type::Color, "r" | "g" | "b" | "a")
                );
                if !ok {
                    self.diags.push(Diagnostic::error(
                        e.span,
                        alloc::format!("`{}` has no field `{field}`", ty.as_str()),
                        "vec2 has .x .y (or .u .v), vec3 has .x .y .z, color has .r .g .b .a",
                    ));
                }
                (Type::Float, rate)
            }
            ExprKind::Call { callee, args } => {
                let mut rate = Rate::Once;
                let mut arg_types = Vec::new();
                for a in args {
                    let (t, r) = self.check(a);
                    arg_types.push(t);
                    rate = rate.max(r);
                }
                if let Some(sig) = core_fn(callee) {
                    if !sig.arity.contains(&args.len()) {
                        self.diags.push(Diagnostic::error(
                            e.span,
                            alloc::format!(
                                "`{callee}` takes {} arguments, but {} were given",
                                describe_arity(sig.arity),
                                args.len()
                            ),
                            "check the argument list",
                        ));
                    }
                    return (sig.ret, rate);
                }
                let (ty, rate, index) = match self.symbols.get(callee) {
                    Some(s) if s.kind == SymbolKind::Fn => {
                        let (ty, index) = (s.ty, s.index);
                        (ty, rate, index)
                    }
                    _ => {
                        self.diags.push(Diagnostic::error(
                            e.span,
                            alloc::format!("unknown function `{callee}`"),
                            "check the spelling, or declare it with `fn`",
                        ));
                        (Type::Float, rate, usize::MAX)
                    }
                };
                if index != usize::MAX {
                    if let Some(f) = self.fns.get(index) {
                        if f.params.len() != args.len() {
                            self.diags.push(Diagnostic::error(
                                e.span,
                                alloc::format!(
                                    "`{callee}` takes {} arguments, but {} were given",
                                    f.params.len(),
                                    args.len()
                                ),
                                "check the argument list against the function declaration",
                            ));
                        }
                    }
                }
                (ty, rate)
            }
            ExprKind::MethodCall { base, args, .. } => {
                let (_, mut rate) = self.check(base);
                for a in args {
                    let (_, r) = self.check(a);
                    rate = rate.max(r);
                }
                // Sim accessors are green - they run per pixel against this
                // device's own coordinates - but sims are not emitted yet, so
                // resolve() has already reported that.
                (Type::Float, Rate::Pixel.min(rate.max(Rate::Pixel)))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let (lt, lr) = self.check(lhs);
                let (rt, rr) = self.check(rhs);
                let rate = lr.max(rr);
                let is_compare = matches!(
                    op,
                    BinOp::Lt
                        | BinOp::Le
                        | BinOp::Gt
                        | BinOp::Ge
                        | BinOp::Eq
                        | BinOp::Ne
                        | BinOp::And
                        | BinOp::Or
                );
                if is_compare {
                    return (Type::Bool, rate);
                }
                // A vector op a scalar is fine; two different vector widths is
                // not, and would otherwise emit silently wrong code.
                let out = if lt.width() >= rt.width() { lt } else { rt };
                if lt.width() > 1 && rt.width() > 1 && lt.width() != rt.width() {
                    self.diags.push(Diagnostic::error(
                        e.span,
                        alloc::format!(
                            "cannot apply `{}` to `{}` and `{}`",
                            op.as_str(),
                            lt.as_str(),
                            rt.as_str()
                        ),
                        "both sides must be the same width, or one of them a scalar",
                    ));
                }
                (out, rate)
            }
            ExprKind::Unary { operand, op } => {
                let (ty, rate) = self.check(operand);
                if *op == UnOp::Not {
                    (Type::Bool, rate)
                } else {
                    (ty, rate)
                }
            }
        }
    }

    /// The first pixel-rate name an expression reads, for the hoisting warning.
    ///
    /// Naming the culprit is the difference between an author fixing it in a
    /// minute and never noticing.
    fn why_pixel(&self, e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if let Some(b) = builtin(name) {
                    if b.rate == Rate::Pixel {
                        return Some(name.clone());
                    }
                }
                match self.symbols.get(name) {
                    Some(s) if s.rate == Rate::Pixel => Some(name.clone()),
                    _ => None,
                }
            }
            ExprKind::Field { base, .. } => self.why_pixel(base),
            ExprKind::Call { args, .. } => args.iter().find_map(|a| self.why_pixel(a)),
            ExprKind::MethodCall { base, args, .. } => self
                .why_pixel(base)
                .or_else(|| args.iter().find_map(|a| self.why_pixel(a))),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.why_pixel(lhs).or_else(|| self.why_pixel(rhs))
            }
            ExprKind::Unary { operand, .. } => self.why_pixel(operand),
            _ => None,
        }
    }
}

fn describe_arity(arity: &[usize]) -> String {
    let mut s = String::new();
    for (i, a) in arity.iter().enumerate() {
        if i > 0 {
            s.push_str(" or ");
        }
        s.push_str(&alloc::format!("{a}"));
    }
    s
}

/// Whether `f` can reach itself through the call graph, and via which name.
///
/// A depth-limited walk rather than a proper strongly-connected-components
/// pass: the call graph of one effect is tiny, and naming *a* function on the
/// cycle is enough for the author to find it.
fn calls_itself(f: &FnDecl, all: &[FnDecl]) -> Option<String> {
    fn reaches(
        target: &str,
        expr: &Expr,
        all: &[FnDecl],
        seen: &mut Vec<String>,
    ) -> Option<String> {
        match &expr.kind {
            ExprKind::Call { callee, args } => {
                if callee == target {
                    return Some(callee.clone());
                }
                for a in args {
                    if let Some(v) = reaches(target, a, all, seen) {
                        return Some(v);
                    }
                }
                if seen.iter().any(|s| s == callee) {
                    return None;
                }
                let next = all.iter().find(|g| &g.name == callee)?;
                seen.push(callee.clone());
                for l in &next.lets {
                    if reaches(target, &l.value, all, seen).is_some() {
                        return Some(callee.clone());
                    }
                }
                if reaches(target, &next.body, all, seen).is_some() {
                    return Some(callee.clone());
                }
                None
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                reaches(target, lhs, all, seen).or_else(|| reaches(target, rhs, all, seen))
            }
            ExprKind::Unary { operand, .. } => reaches(target, operand, all, seen),
            ExprKind::Field { base, .. } => reaches(target, base, all, seen),
            ExprKind::MethodCall { base, args, .. } => reaches(target, base, all, seen)
                .or_else(|| args.iter().find_map(|a| reaches(target, a, all, seen))),
            _ => None,
        }
    }

    let mut seen = alloc::vec![f.name.clone()];
    for l in &f.lets {
        if let Some(v) = reaches(&f.name, &l.value, all, &mut seen) {
            return Some(v);
        }
    }
    reaches(&f.name, &f.body, all, &mut seen)
}
