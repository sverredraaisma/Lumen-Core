//! The abstract syntax tree.
//!
//! **This is public API, not a compiler internal.** Text is the canonical format
//! and the node editor is a view over it, so the editor builds and mutates these
//! types directly and then calls [`crate::fmt`] to write a diffable file back
//! out. Keeping the AST public is what stops the editor inventing a private
//! representation that the text cannot express.
//!
//! Every node carries a [`Span`] so a diagnostic, a hover, or an editor
//! selection can point back at the source that produced it.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::diag::Span;
use crate::lex::{Comment, Unit};

/// A whole `.lfx` file. Self-contained: no imports, no external references.
#[derive(Clone, PartialEq, Debug)]
pub struct File {
    /// The `lumen N` header — the language version.
    pub language_version: u32,
    pub decls: Vec<Decl>,
    /// Every comment in the file, in source order.
    ///
    /// Held on the file rather than attached to nodes because a comment can sit
    /// anywhere, including places the grammar has no node for — between two
    /// declarations, after the last one, inside a blank stretch. The formatter
    /// places them by span, which handles all of those without the AST needing
    /// a slot for each.
    pub comments: Vec<Comment>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Decl {
    Effect(Effect),
    Palette(Palette),
    Curve(Curve),
    Fn(FnDecl),
}

/// Device capabilities an effect can require.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    /// Mapping quality `mapped`.
    Mapped,
    /// Mapping quality `rough` or better.
    Rough,
    Rgbw,
    Cct,
    Audio,
    Imu,
    Grid,
    Input,
}

impl Cap {
    // Inherent rather than the `FromStr` trait: these return `Option`, not
    // `Result`, because "not a capability" is a normal parse outcome that the
    // caller turns into a diagnostic with a span, not an error type.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Cap> {
        Some(match s {
            "mapped" => Cap::Mapped,
            "rough" => Cap::Rough,
            "rgbw" => Cap::Rgbw,
            "cct" => Cap::Cct,
            "audio" => Cap::Audio,
            "imu" => Cap::Imu,
            "grid" => Cap::Grid,
            "input" => Cap::Input,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Cap::Mapped => "mapped",
            Cap::Rough => "rough",
            Cap::Rgbw => "rgbw",
            Cap::Cct => "cct",
            Cap::Audio => "audio",
            Cap::Imu => "imu",
            Cap::Grid => "grid",
            Cap::Input => "input",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Effect {
    pub name: String,
    pub version: Option<u32>,
    pub author: Option<String>,
    pub stdlib: Option<u32>,
    pub requires: Vec<Cap>,
    /// Preferred frame rate. Advisory: the frame grid already constrains the
    /// options, so an effect that merely prefers a rate should not be able to
    /// refuse to run.
    pub fps: Option<u32>,
    /// `budget n on <class>` — an optional, machine-checkable claim about cost.
    /// Enforced in CI for shared effects, ignored for personal ones.
    pub budgets: Vec<BudgetClaim>,
    pub params: Vec<Param>,
    pub channels: Vec<Channel>,
    pub lets: Vec<Binding>,
    pub masks: Vec<Binding>,
    pub states: Vec<StateDecl>,
    pub layers: Vec<Layer>,
    pub sims: Vec<Sim>,
    pub fns: Vec<FnDecl>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct BudgetClaim {
    pub instructions: u32,
    pub device_class: String,
    pub span: Span,
}

/// A declared type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    /// The default numeric type, `q16`.
    Float,
    /// `i32`. Compile-time in most positions.
    Int,
    /// `q16` 0 or 1, so masks and `select` need no separate path.
    Bool,
    /// `q16` radians. Literals accept `deg` or `rad`, which prevents the classic
    /// unit bug.
    Angle,
    Vec2,
    Vec3,
    /// Always linear. An effect never applies gamma and never sees a
    /// gamma-encoded value.
    Color,
    Palette,
    Curve,
    /// One element of a simulation, as bound by a `foreach`.
    ///
    /// Its fields are whatever the block assigns, and each is a [`Type::Vec3`].
    /// The grammar does not say what an element's fields are typed as; every
    /// accessor takes or returns a point or a vector, so that is the reading it
    /// implies. Recorded as an open question rather than settled - a scalar
    /// field is a plausible thing to want and cannot currently be written.
    Element,
    /// A simulation's broadcast state, reached through a `sim<..>` channel.
    ///
    /// A handle like [`Type::Palette`] and [`Type::Curve`]: it names something
    /// the program can ask questions of, and is not itself a value. Deliberately
    /// absent from [`Type::from_str`] — there is no `param x : sim`, because a
    /// simulation arrives on a channel and nowhere else.
    Sim,
}

impl Type {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Type> {
        Some(match s {
            "float" => Type::Float,
            "int" => Type::Int,
            "bool" => Type::Bool,
            "angle" => Type::Angle,
            "vec2" => Type::Vec2,
            "vec3" => Type::Vec3,
            "color" => Type::Color,
            "palette" => Type::Palette,
            "curve" => Type::Curve,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Type::Element => "element",
            Type::Sim => "sim",
            Type::Float => "float",
            Type::Int => "int",
            Type::Bool => "bool",
            Type::Angle => "angle",
            Type::Vec2 => "vec2",
            Type::Vec3 => "vec3",
            Type::Color => "color",
            Type::Palette => "palette",
            Type::Curve => "curve",
        }
    }

    /// Registers this type occupies.
    pub fn width(self) -> usize {
        match self {
            Type::Vec2 => 2,
            Type::Vec3 | Type::Color => 3,
            _ => 1,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub default: Expr,
    /// Required for `float` params: it drives the slider in both apps, and a
    /// parameter with no bounds cannot be presented in a UI or bound to a MIDI
    /// CC.
    pub range: Option<(Expr, Expr)>,
    pub unit: Option<Unit>,
    pub step: Option<Expr>,
    /// A human name, where the identifier is terse.
    pub label: Option<String>,
    pub span: Span,
}

/// What a channel carries.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChanType {
    AudioBands,
    AudioBeat,
    Sim(String),
    Sensor(String),
    Value,
    Vec3,
    /// A length-prefixed UTF-8 blob. Defaults to 64 bytes.
    ///
    /// Exists because a scrolling message needs a channel a `value` cannot
    /// express.
    Text(u32),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Channel {
    pub name: String,
    pub ty: ChanType,
    /// Staleness window in milliseconds.
    ///
    /// **`hold 0` means never stale** — right for a value pushed only on change,
    /// like a scrolling message or a mode selector, where treating silence as
    /// failure would be wrong. Anything sampled continuously should set a real
    /// window.
    pub hold_ms: Option<u32>,
    /// What the value decays to when the producer dies, so a dead audio source
    /// fades the lights to steady rather than freezing them mid-beat.
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Binding {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct StateDecl {
    pub name: String,
    pub ty: Type,
    pub init: Expr,
    pub span: Span,
}

/// How a layer combines with what is beneath it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blend {
    Normal,
    Add,
    Multiply,
    Screen,
    Overlay,
    Max,
    Min,
    Difference,
}

impl Blend {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Blend> {
        Some(match s {
            "normal" => Blend::Normal,
            "add" => Blend::Add,
            "multiply" => Blend::Multiply,
            "screen" => Blend::Screen,
            "overlay" => Blend::Overlay,
            "max" => Blend::Max,
            "min" => Blend::Min,
            "difference" => Blend::Difference,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Blend::Normal => "normal",
            Blend::Add => "add",
            Blend::Multiply => "multiply",
            Blend::Screen => "screen",
            Blend::Overlay => "overlay",
            Blend::Max => "max",
            Blend::Min => "min",
            Blend::Difference => "difference",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Layer {
    pub name: String,
    pub mask: Option<String>,
    pub blend: Blend,
    pub opacity: Option<Expr>,
    pub lets: Vec<Binding>,
    /// Assignments. Every layer must assign `color`.
    pub assigns: Vec<Assign>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Assign {
    pub target: String,
    /// A field, for `pos.x = ...`.
    pub field: Option<String>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Sim {
    pub name: String,
    /// **Compile-time constants.** `count = 64` sizes an array and cannot be a
    /// `param`, because the `sim` VM profile has no dynamic allocation.
    pub args: Vec<(String, Expr)>,
    pub body: Vec<SimStmt>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub enum SimStmt {
    Let(Binding),
    Assign(Assign),
    If {
        cond: Expr,
        then: Vec<SimStmt>,
        otherwise: Vec<SimStmt>,
        span: Span,
    },
    ForEach {
        binding: String,
        over: String,
        body: Vec<SimStmt>,
        span: Span,
    },
}

/// The text form of an encapsulated node group.
///
/// Always inlined: no recursion, no function pointers, no dynamic dispatch.
#[derive(Clone, PartialEq, Debug)]
pub struct FnDecl {
    pub name: String,
    pub params: Vec<(String, Type)>,
    pub ret: Option<Type>,
    pub lets: Vec<Binding>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Palette {
    pub name: String,
    /// Defaults to `oklab`. Stops resolve to a lookup table at compile time, so
    /// the choice of space costs nothing at runtime.
    pub space: ColorSpace,
    pub stops: Vec<Stop>,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorSpace {
    Oklab,
    Oklch,
    Hsv,
    LinearRgb,
}

impl ColorSpace {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<ColorSpace> {
        Some(match s {
            "oklab" => ColorSpace::Oklab,
            "oklch" => ColorSpace::Oklch,
            "hsv" => ColorSpace::Hsv,
            "linear_rgb" => ColorSpace::LinearRgb,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ColorSpace::Oklab => "oklab",
            ColorSpace::Oklch => "oklch",
            ColorSpace::Hsv => "hsv",
            ColorSpace::LinearRgb => "linear_rgb",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Stop {
    pub position: f64,
    pub color: Expr,
    pub span: Span,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Curve {
    pub name: String,
    pub points: Vec<(f64, f64)>,
    pub span: Span,
}

/// A binary operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }

    /// Binding power. Higher binds tighter.
    pub fn precedence(self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Ne => 3,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::Add | BinOp::Sub => 5,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 6,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum ExprKind {
    /// A numeric literal, already converted: degrees to radians, percent to a
    /// fraction. `unit` is retained for the unit check on `param`.
    Number {
        value: f64,
        unit: Option<Unit>,
    },
    /// `#RRGGBB[AA]`, as linear RGBA in 0..1.
    Color([f64; 4]),
    Str(String),
    Ident(String),
    /// `a.b`, for `.x .y .z` and `.u .v`, and for sim accessors.
    Field {
        base: Box<Expr>,
        field: String,
    },
    Call {
        callee: String,
        args: Vec<Expr>,
    },
    /// `<sim>.influence(p, r)` and friends.
    MethodCall {
        base: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
}

/// The value of a compile-time constant expression, or `None` if it is not one.
///
/// This is the *only* definition of what counts as constant. The emitter folds
/// parameter defaults with it and the resolver rejects anything it returns
/// `None` for, so the two cannot drift: before they shared this, a default like
/// `0.5 * 2` silently compiled to zero, with no diagnostic and no way for an
/// author to tell.
///
/// The width matches the type — one element for a number, three for a colour —
/// because a parameter default has to fill however many registers the parameter
/// occupies.
pub fn const_value(e: &Expr) -> Option<alloc::vec::Vec<f64>> {
    match &e.kind {
        ExprKind::Number { value, .. } => Some(alloc::vec![*value]),
        ExprKind::Color(c) => Some(alloc::vec![c[0], c[1], c[2]]),
        ExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } => Some(const_value(operand)?.iter().map(|v| -v).collect()),
        ExprKind::Call { callee, args } if callee == "rgb" || callee == "vec3" => {
            let mut out = alloc::vec::Vec::new();
            for a in args {
                // Each argument contributes its first component, so a nested
                // colour would be a type error rather than a silent truncation.
                out.push(*const_value(a)?.first()?);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Whether `name` is read anywhere in `e`.
///
/// Used to work out when a `let` inside a layer is dead, so its register can be
/// handed back. A conservative answer is always safe here: reporting a mention
/// that is not there costs a register, reporting none that is there would emit a
/// read of a register something else has since overwritten.
pub fn mentions(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Number { .. } | ExprKind::Color(_) | ExprKind::Str(_) => false,
        ExprKind::Field { base, .. } => mentions(base, name),
        ExprKind::Call { args, .. } => args.iter().any(|a| mentions(a, name)),
        ExprKind::MethodCall { base, args, .. } => {
            mentions(base, name) || args.iter().any(|a| mentions(a, name))
        }
        ExprKind::Binary { lhs, rhs, .. } => mentions(lhs, name) || mentions(rhs, name),
        ExprKind::Unary { operand, .. } => mentions(operand, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one expression out of a layer's `color`, so `mentions` is tested
    /// against trees the parser actually builds rather than ones hand-assembled
    /// here. A hand-built tree can be a shape the grammar cannot produce, and
    /// then the test proves nothing about real source.
    fn expr_of(text: &str) -> Expr {
        let src = alloc::format!(
            "lumen 1
effect \"x\" {{
  version 1
  author \"t\"
  stdlib 1
  fps 60
  layer l {{ color = {text} }}
}}
"
        );
        let (file, diags) = crate::parse::parse(&src);
        assert!(!diags.has_errors(), "{}", diags.render(&src));
        let file = file.expect("parsed");
        let Decl::Effect(e) = &file.decls[0] else {
            panic!("expected an effect")
        };
        e.layers[0].assigns[0].value.clone()
    }

    #[test]
    fn mentions_finds_a_name_through_every_kind_of_expression() {
        // One case per `ExprKind`, because the walker missing a variant is
        // silent: the value looks dead, its register is handed back, and
        // something else overwrites it before the read.
        for text in [
            "wanted",                        // Ident
            "wanted.x",                      // Field
            "rgb(wanted, 0, 0)",             // Call
            "sim.influence(wanted, 1)",      // MethodCall through an argument
            "wanted + 1",                    // Binary, left
            "1 + wanted",                    // Binary, right
            "-wanted",                       // Unary
            "rgb(max(0, wanted * 2), 0, 0)", // nested
        ] {
            assert!(
                mentions(&expr_of(text), "wanted"),
                "`{text}` mentions `wanted`"
            );
        }
    }

    #[test]
    fn mentions_says_no_when_the_name_is_absent() {
        for text in [
            "1.5",                     // Number
            "#204080",                 // Color
            "other",                   // a different Ident
            "other.x",                 // Field
            "rgb(other, 0, 0)",        // Call
            "other + 1",               // Binary
            "-other",                  // Unary
            "sim.influence(other, 1)", // MethodCall
        ] {
            assert!(
                !mentions(&expr_of(text), "wanted"),
                "`{text}` does not mention `wanted`"
            );
        }
    }

    #[test]
    fn a_name_that_is_only_a_prefix_does_not_count() {
        // `want` is not `wanted`. A prefix match here would keep a binding
        // alive for no reason, which is merely wasteful — but the reverse, a
        // suffix or substring match, would be the dangerous direction, so pin
        // that the comparison is whole-name.
        assert!(!mentions(&expr_of("want + 1"), "wanted"));
        assert!(!mentions(&expr_of("wantedmore + 1"), "wanted"));
    }

    #[test]
    fn every_capability_maps_both_ways() {
        for c in [
            Cap::Mapped,
            Cap::Rough,
            Cap::Rgbw,
            Cap::Cct,
            Cap::Audio,
            Cap::Imu,
            Cap::Grid,
            Cap::Input,
        ] {
            assert_eq!(Cap::from_str(c.as_str()), Some(c));
        }
        assert_eq!(Cap::from_str("teleport"), None);
    }

    #[test]
    fn every_type_maps_both_ways_and_knows_its_width() {
        for t in [
            Type::Float,
            Type::Int,
            Type::Bool,
            Type::Angle,
            Type::Vec2,
            Type::Vec3,
            Type::Color,
            Type::Palette,
            Type::Curve,
        ] {
            assert_eq!(Type::from_str(t.as_str()), Some(t));
        }
        assert_eq!(Type::from_str("matrix"), None);
        assert_eq!(Type::Float.width(), 1);
        assert_eq!(Type::Vec2.width(), 2);
        assert_eq!(Type::Vec3.width(), 3);
        assert_eq!(Type::Color.width(), 3);
    }

    #[test]
    fn every_blend_mode_maps_both_ways() {
        for b in [
            Blend::Normal,
            Blend::Add,
            Blend::Multiply,
            Blend::Screen,
            Blend::Overlay,
            Blend::Max,
            Blend::Min,
            Blend::Difference,
        ] {
            assert_eq!(Blend::from_str(b.as_str()), Some(b));
        }
        assert_eq!(Blend::from_str("dodge"), None);
    }

    #[test]
    fn every_colour_space_maps_both_ways() {
        for c in [
            ColorSpace::Oklab,
            ColorSpace::Oklch,
            ColorSpace::Hsv,
            ColorSpace::LinearRgb,
        ] {
            assert_eq!(ColorSpace::from_str(c.as_str()), Some(c));
        }
        assert_eq!(ColorSpace::from_str("cmyk"), None);
    }

    #[test]
    fn operator_precedence_matches_the_usual_expectations() {
        assert!(BinOp::Mul.precedence() > BinOp::Add.precedence());
        assert!(BinOp::Add.precedence() > BinOp::Lt.precedence());
        assert!(BinOp::Lt.precedence() > BinOp::Eq.precedence());
        assert!(BinOp::Eq.precedence() > BinOp::And.precedence());
        assert!(BinOp::And.precedence() > BinOp::Or.precedence());
    }

    #[test]
    fn every_operator_prints() {
        for op in [
            BinOp::Add,
            BinOp::Sub,
            BinOp::Mul,
            BinOp::Div,
            BinOp::Rem,
            BinOp::Lt,
            BinOp::Le,
            BinOp::Gt,
            BinOp::Ge,
            BinOp::Eq,
            BinOp::Ne,
            BinOp::And,
            BinOp::Or,
        ] {
            assert!(!op.as_str().is_empty());
        }
    }
}
