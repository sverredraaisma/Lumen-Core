//! The canonical formatter.
//!
//! Text is the canonical format and the node editor is a view over it, so a
//! round trip through the graph editor has to leave a file a human would have
//! written — not an editor-private blob, and not the same content with the
//! whitespace shuffled. This module is what makes that true: the editor mutates
//! the [`crate::ast`] and calls [`format`], and the diff shows only what changed.
//!
//! Formatting is **idempotent**: formatting formatted output changes nothing.
//! There is a test for it, because a formatter that is not idempotent produces
//! diff churn on every save and people turn it off.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::*;
use crate::diag::Span;
use crate::lex::Comment;

const INDENT: &str = "  ";

/// Emits comments back into the output as their positions come round.
///
/// The formatter walks the tree in source order, so a cursor over the comments
/// is enough: before writing anything at byte `at`, flush every comment that
/// started earlier. That handles comments in places the grammar has no node
/// for (between declarations, after the last one, in a blank stretch) without
/// the AST needing a slot for each.
struct Comments<'a> {
    items: &'a [Comment],
    next: usize,
}

impl<'a> Comments<'a> {
    fn new(items: &'a [Comment]) -> Comments<'a> {
        Comments { items, next: 0 }
    }

    /// Write out every comment that began before `at`.
    fn flush_before(&mut self, out: &mut String, at: usize, depth: usize) {
        while let Some(c) = self.items.get(self.next) {
            if c.span.start >= at {
                return;
            }
            self.next += 1;
            // A comment that trailed code goes on its own line here. Keeping it
            // trailing would need to know which line the code landed on after
            // reformatting, and a comment on the wrong line reads as explaining
            // something it does not.
            indent(out, depth);
            if c.text.is_empty() {
                out.push_str("#\n");
            } else {
                out.push_str(&format!("# {}\n", c.text));
            }
        }
    }

    /// Everything left, for the end of the file.
    fn flush_rest(&mut self, out: &mut String) {
        self.flush_before(out, usize::MAX, 0);
    }
}

/// Render a file back to canonical `.lfx` text.
pub fn format(file: &File) -> String {
    let mut out = String::new();
    let mut comments = Comments::new(&file.comments);
    out.push_str(&format!("lumen {}\n", file.language_version));
    for d in &file.decls {
        let span = decl_span(d);
        out.push('\n');
        comments.flush_before(&mut out, span.start, 0);
        match d {
            Decl::Effect(e) => effect(&mut out, e, &mut comments),
            Decl::Palette(p) => palette(&mut out, p),
            Decl::Curve(c) => curve(&mut out, c),
            Decl::Fn(f) => fn_decl(&mut out, f, 0),
        }
    }
    // Whatever trails the last declaration. Dropping it would lose exactly the
    // comment people write at the bottom of a file to explain the whole thing.
    if comments.next < comments.items.len() {
        out.push('\n');
        comments.flush_rest(&mut out);
    }
    out
}

fn decl_span(d: &Decl) -> Span {
    match d {
        Decl::Effect(e) => e.span,
        Decl::Palette(p) => p.span,
        Decl::Curve(c) => c.span,
        Decl::Fn(f) => f.span,
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

fn effect(out: &mut String, e: &Effect, comments: &mut Comments<'_>) {
    out.push_str(&format!("effect {} {{\n", quote(&e.name)));

    // Metadata first, in a fixed order, so two files that differ only in the
    // order the author typed them format identically.
    if let Some(v) = e.version {
        indent(out, 1);
        out.push_str(&format!("version {v}\n"));
    }
    if let Some(a) = &e.author {
        indent(out, 1);
        out.push_str(&format!("author {}\n", quote(a)));
    }
    if let Some(s) = e.stdlib {
        indent(out, 1);
        out.push_str(&format!("stdlib {s}\n"));
    }
    if !e.requires.is_empty() {
        indent(out, 1);
        let caps: Vec<&str> = e.requires.iter().map(|c| c.as_str()).collect();
        out.push_str(&format!("requires {}\n", caps.join(", ")));
    }
    if let Some(f) = e.fps {
        indent(out, 1);
        out.push_str(&format!("fps {f}\n"));
    }
    for b in &e.budgets {
        indent(out, 1);
        out.push_str(&format!(
            "budget {} on {}\n",
            b.instructions, b.device_class
        ));
    }

    section(out, &e.params, |out, p| {
        comments.flush_before(out, p.span.start, 1);
        indent(out, 1);
        out.push_str(&format!(
            "param {} : {} = {}",
            p.name,
            p.ty.as_str(),
            expr(&p.default)
        ));
        if let Some((lo, hi)) = &p.range {
            out.push_str(&format!(" range {}..{}", expr(lo), expr(hi)));
        }
        if let Some(u) = p.unit {
            out.push_str(&format!(" unit {}", u.as_str()));
        }
        if let Some(s) = &p.step {
            out.push_str(&format!(" step {}", expr(s)));
        }
        if let Some(l) = &p.label {
            out.push_str(&format!(" label {}", quote(l)));
        }
        out.push('\n');
    });

    section(out, &e.channels, |out, c| {
        comments.flush_before(out, c.span.start, 1);
        indent(out, 1);
        out.push_str(&format!("channel {} : {}", c.name, chan_type(&c.ty)));
        if let Some(h) = c.hold_ms {
            out.push_str(&format!(" hold {h}"));
        }
        if let Some(d) = &c.default {
            out.push_str(&format!(" default {}", expr(d)));
        }
        out.push('\n');
    });

    section(out, &e.lets, |out, b| {
        comments.flush_before(out, b.span.start, 1);
        indent(out, 1);
        out.push_str(&format!("let {} = {}\n", b.name, expr(&b.value)));
    });

    section(out, &e.masks, |out, b| {
        comments.flush_before(out, b.span.start, 1);
        indent(out, 1);
        out.push_str(&format!("mask {} = {}\n", b.name, expr(&b.value)));
    });

    section(out, &e.states, |out, s| {
        comments.flush_before(out, s.span.start, 1);
        indent(out, 1);
        out.push_str(&format!(
            "state {} : {} = {}\n",
            s.name,
            s.ty.as_str(),
            expr(&s.init)
        ));
    });

    for f in &e.fns {
        out.push('\n');
        comments.flush_before(out, f.span.start, 1);
        fn_decl(out, f, 1);
    }

    for l in &e.layers {
        out.push('\n');
        comments.flush_before(out, l.span.start, 1);
        layer(out, l, comments);
    }

    // Anything still inside the braces belongs to this effect, not to whatever
    // declaration follows it.
    comments.flush_before(out, e.span.end, 1);
    out.push_str("}\n");
}

/// Emit a blank line before a group, but only when the group has content.
fn section<T>(out: &mut String, items: &[T], mut each: impl FnMut(&mut String, &T)) {
    if items.is_empty() {
        return;
    }
    out.push('\n');
    for it in items {
        each(out, it);
    }
}

fn layer(out: &mut String, l: &Layer, comments: &mut Comments<'_>) {
    indent(out, 1);
    out.push_str(&format!("layer {}", l.name));
    if let Some(m) = &l.mask {
        out.push_str(&format!(" mask({m})"));
    }
    if l.blend != Blend::Normal {
        out.push_str(&format!(" blend {}", l.blend.as_str()));
    }
    if let Some(o) = &l.opacity {
        out.push_str(&format!(" opacity {}", expr(o)));
    }
    out.push_str(" {\n");
    for b in &l.lets {
        comments.flush_before(out, b.span.start, 2);
        indent(out, 2);
        out.push_str(&format!("let {} = {}\n", b.name, expr(&b.value)));
    }
    for a in &l.assigns {
        comments.flush_before(out, a.span.start, 2);
        indent(out, 2);
        match &a.field {
            Some(f) => out.push_str(&format!("{}.{} = {}\n", a.target, f, expr(&a.value))),
            None => out.push_str(&format!("{} = {}\n", a.target, expr(&a.value))),
        }
    }
    comments.flush_before(out, l.span.end, 2);
    indent(out, 1);
    out.push_str("}\n");
}

fn fn_decl(out: &mut String, f: &FnDecl, depth: usize) {
    indent(out, depth);
    let params: Vec<String> = f
        .params
        .iter()
        .map(|(n, t)| format!("{n} : {}", t.as_str()))
        .collect();
    out.push_str(&format!("fn {}({})", f.name, params.join(", ")));
    if let Some(r) = f.ret {
        out.push_str(&format!(" -> {}", r.as_str()));
    }
    out.push_str(" {\n");
    for b in &f.lets {
        indent(out, depth + 1);
        out.push_str(&format!("let {} = {}\n", b.name, expr(&b.value)));
    }
    indent(out, depth + 1);
    out.push_str(&format!("return {}\n", expr(&f.body)));
    indent(out, depth);
    out.push_str("}\n");
}

fn palette(out: &mut String, p: &Palette) {
    out.push_str(&format!("palette {} {{\n", p.name));
    if p.space != ColorSpace::Oklab {
        indent(out, 1);
        out.push_str(&format!("space {}\n", p.space.as_str()));
    }
    for s in &p.stops {
        indent(out, 1);
        out.push_str(&format!("{} {}\n", number(s.position), expr(&s.color)));
    }
    out.push_str("}\n");
}

fn curve(out: &mut String, c: &Curve) {
    out.push_str(&format!("curve {} {{\n", c.name));
    for (x, y) in &c.points {
        indent(out, 1);
        out.push_str(&format!("{} {}\n", number(*x), number(*y)));
    }
    out.push_str("}\n");
}

fn chan_type(t: &ChanType) -> String {
    match t {
        ChanType::AudioBands => "audio_bands".to_string(),
        ChanType::AudioBeat => "audio_beat".to_string(),
        ChanType::Sim(n) => format!("sim<{n}>"),
        ChanType::Sensor(n) => format!("sensor<{n}>"),
        ChanType::Value => "value".to_string(),
        ChanType::Vec3 => "vec3".to_string(),
        ChanType::Text(64) => "text".to_string(),
        ChanType::Text(n) => format!("text({n})"),
    }
}

/// Render an expression, parenthesising only where precedence requires it.
///
/// Printing every subexpression in brackets would round-trip correctly and be
/// unreadable, which defeats the point of text being canonical.
pub fn expr(e: &Expr) -> String {
    expr_prec(e, 0)
}

fn expr_prec(e: &Expr, parent: u8) -> String {
    match &e.kind {
        ExprKind::Number { value, unit } => match unit {
            // Units are converted at parse time, so printing the converted value
            // with its original suffix would be wrong. Print what the value is.
            Some(crate::lex::Unit::Deg)
            | Some(crate::lex::Unit::Ms)
            | Some(crate::lex::Unit::Percent) => number(*value),
            Some(u) => format!("{}{}", number(*value), u.as_str()),
            None => number(*value),
        },
        ExprKind::Color(c) => hex(*c),
        ExprKind::Str(s) => quote(s),
        ExprKind::Ident(n) => n.clone(),
        ExprKind::Field { base, field } => format!("{}.{field}", expr_prec(base, 10)),
        ExprKind::Call { callee, args } => {
            let a: Vec<String> = args.iter().map(expr).collect();
            format!("{callee}({})", a.join(", "))
        }
        ExprKind::MethodCall { base, method, args } => {
            let a: Vec<String> = args.iter().map(expr).collect();
            format!("{}.{method}({})", expr_prec(base, 10), a.join(", "))
        }
        ExprKind::Unary { op, operand } => {
            let sym = match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
            };
            format!("{sym}{}", expr_prec(operand, 9))
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let p = op.precedence();
            let text = format!(
                "{} {} {}",
                expr_prec(lhs, p),
                op.as_str(),
                // The right operand needs brackets at equal precedence, or
                // `a - (b - c)` reprints as `a - b - c`.
                expr_prec(rhs, p + 1)
            );
            if p < parent {
                format!("({text})")
            } else {
                text
            }
        }
    }
}

/// Print a number without a trailing `.0`, and without scientific notation.
fn number(v: f64) -> String {
    if v == (v as i64) as f64 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    // Six decimal places is more than Q16.16 can represent, so nothing is lost.
    let mut s = format!("{v:.6}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// Print a linear colour back as the sRGB hex literal it came from.
fn hex(c: [f64; 4]) -> String {
    let to_srgb = |l: f64| -> u8 {
        let s = if l <= 0.003_130_8 {
            l * 12.92
        } else {
            // The inverse of the parser's transfer function, to five places.
            1.055 * powf(l, 1.0 / 2.4) - 0.055
        };
        (s.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    };
    let (r, g, b) = (to_srgb(c[0]), to_srgb(c[1]), to_srgb(c[2]));
    let a = (c[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

fn powf(x: f64, y: f64) -> f64 {
    crate::parse::srgb_pow(x, y)
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
