//! The parser.
//!
//! Recursive descent, with Pratt-style precedence climbing for expressions.
//!
//! **An unknown construct is an error, never skipped.** Silently ignoring
//! something the compiler does not recognise produces effects that render subtly
//! wrong on old software, which is far worse than a refusal to compile — so
//! every branch here ends in a diagnostic rather than a `continue`.
//!
//! The parser recovers at statement boundaries so one run reports several
//! problems. It never invents nodes to keep going: a failed item is dropped, not
//! guessed at.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ast::*;
use crate::diag::{Diagnostic, Diagnostics, Span};
use crate::lex::{lex_with_comments, Tok, Token, Unit};

/// Parse a `.lfx` source file.
///
/// Returns whatever could be parsed alongside every diagnostic found. Check
/// [`Diagnostics::has_errors`] before trusting the tree.
pub fn parse(src: &str) -> (Option<File>, Diagnostics) {
    let (tokens, comments, lex_errs) = lex_with_comments(src);
    let mut p = Parser {
        toks: tokens,
        at: 0,
        diags: Diagnostics::new(),
    };
    p.diags.extend(lex_errs);
    let mut file = p.file();
    if let Some(f) = file.as_mut() {
        f.comments = comments;
    }
    (file, p.diags)
}

struct Parser {
    toks: Vec<Token>,
    at: usize,
    diags: Diagnostics,
}

impl Parser {
    // ---- token helpers ----------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.at.min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.at.min(self.toks.len() - 1)].span
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.at.min(self.toks.len() - 1)].tok.clone();
        if self.at < self.toks.len() - 1 {
            self.at += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == want {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error(&mut self, span: Span, msg: impl Into<String>, help: impl Into<String>) {
        self.diags.push(Diagnostic::error(span, msg, help));
    }

    fn expect(&mut self, want: &Tok) -> bool {
        if self.eat(want) {
            return true;
        }
        let found = self.peek().describe();
        let span = self.span();
        self.error(
            span,
            alloc::format!("expected {}, found {found}", want.describe()),
            alloc::format!("add {} here", want.describe()),
        );
        false
    }

    /// Turn the description of an identifier into a plausible example of one.
    ///
    /// Callers pass an article phrase — "a parameter name", "an array name" —
    /// because it reads correctly in `expected {what}, found ...`. Interpolating
    /// that same phrase into the help line does not: every identifier error in
    /// the language used to end ``must be a name like `my_a parameter name` ``.
    ///
    /// Dropping the article and the redundant trailing "name" turns each phrase
    /// into the identifier a person would actually have written there.
    fn example_name(what: &str) -> String {
        let bare = what
            .strip_prefix("an ")
            .or_else(|| what.strip_prefix("a "))
            .unwrap_or(what);
        // "a parameter name" -> "parameter", but "a name" stays "name".
        let bare = match bare.strip_suffix(" name") {
            Some(trimmed) if !trimmed.is_empty() => trimmed,
            _ => bare,
        };
        let mut out = String::from("my_");
        for ch in bare.chars() {
            out.push(if ch == ' ' { '_' } else { ch });
        }
        out
    }

    /// Consume an identifier, or report and return `None`.
    fn ident(&mut self, what: &str) -> Option<String> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                Some(name)
            }
            other => {
                let span = self.span();
                self.error(
                    span,
                    alloc::format!("expected {what}, found {}", other.describe()),
                    alloc::format!("{what} must be a name like `{}`", Self::example_name(what)),
                );
                None
            }
        }
    }

    fn string(&mut self, what: &str) -> Option<String> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                Some(s)
            }
            other => {
                let span = self.span();
                self.error(
                    span,
                    alloc::format!("expected {what}, found {}", other.describe()),
                    alloc::format!("{what} is written in double quotes"),
                );
                None
            }
        }
    }

    /// Consume a plain integer literal.
    fn integer(&mut self, what: &str) -> Option<u32> {
        match self.peek().clone() {
            Tok::Number { text, unit } => {
                let span = self.span();
                self.bump();
                if unit.is_some() {
                    self.error(
                        span,
                        alloc::format!("{what} may not have a unit"),
                        "write a plain number",
                    );
                }
                match text.parse::<f64>() {
                    Ok(v) if v >= 0.0 && v <= u32::MAX as f64 && v == (v as u32) as f64 => {
                        Some(v as u32)
                    }
                    _ => {
                        self.error(
                            span,
                            alloc::format!("{what} must be a whole number"),
                            "write a non-negative integer",
                        );
                        None
                    }
                }
            }
            other => {
                let span = self.span();
                self.error(
                    span,
                    alloc::format!("expected {what}, found {}", other.describe()),
                    alloc::format!("{what} is a whole number"),
                );
                None
            }
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.bump();
        }
    }

    /// Require the end of a statement.
    fn end_of_statement(&mut self) {
        if matches!(self.peek(), Tok::Newline) {
            self.bump();
        } else if !matches!(self.peek(), Tok::Eof | Tok::RBrace) {
            let found = self.peek().describe();
            let span = self.span();
            self.error(
                span,
                alloc::format!("unexpected {found} after the end of a statement"),
                "statements end at a newline; put this on its own line",
            );
            self.recover_to_statement_end();
        }
    }

    /// Skip to the next statement boundary so one bad line does not cascade.
    fn recover_to_statement_end(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.peek() {
                Tok::Eof => return,
                Tok::Newline if depth <= 0 => {
                    self.bump();
                    return;
                }
                Tok::RBrace if depth <= 0 => return,
                Tok::LBrace => depth += 1,
                Tok::RBrace => depth -= 1,
                _ => {}
            }
            self.bump();
        }
    }

    // ---- file -------------------------------------------------------------

    fn file(&mut self) -> Option<File> {
        let start = self.span();
        self.skip_newlines();

        // `lumen N` header.
        let language_version = match self.peek().clone() {
            Tok::Ident(k) if k == "lumen" => {
                self.bump();
                let v = self.integer("the language version")?;
                self.end_of_statement();
                v
            }
            _ => {
                let span = self.span();
                self.error(
                    span,
                    "missing `lumen` version header",
                    "every effect file starts with a line like `lumen 1`",
                );
                return None;
            }
        };

        let mut decls = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_eof() {
                break;
            }
            let before = self.at;
            match self.decl() {
                Some(d) => decls.push(d),
                None => {
                    // Guarantee progress even when a declaration failed at the
                    // very first token, or this loop never terminates.
                    if self.at == before {
                        self.bump();
                    }
                    self.recover_to_statement_end();
                }
            }
        }

        Some(File {
            language_version,
            decls,
            comments: Vec::new(),
            span: start.merge(self.span()),
        })
    }

    fn decl(&mut self) -> Option<Decl> {
        let span = self.span();
        let keyword = match self.peek().clone() {
            Tok::Ident(k) => k,
            other => {
                self.error(
                    span,
                    alloc::format!("expected a declaration, found {}", other.describe()),
                    "a file contains `effect`, `palette`, `curve` and `fn` declarations",
                );
                return None;
            }
        };
        match keyword.as_str() {
            "effect" => self.effect().map(Decl::Effect),
            "palette" => self.palette().map(Decl::Palette),
            "curve" => self.curve().map(Decl::Curve),
            "fn" => self.fn_decl().map(Decl::Fn),
            other => {
                self.error(
                    span,
                    alloc::format!("unknown declaration `{other}`"),
                    "a file contains `effect`, `palette`, `curve` and `fn` declarations",
                );
                None
            }
        }
    }

    // ---- effect -----------------------------------------------------------

    fn effect(&mut self) -> Option<Effect> {
        let start = self.span();
        self.bump(); // `effect`
        let name = self.string("the effect name")?;
        if !self.expect(&Tok::LBrace) {
            return None;
        }

        let mut e = Effect {
            name,
            version: None,
            author: None,
            stdlib: None,
            requires: Vec::new(),
            fps: None,
            budgets: Vec::new(),
            params: Vec::new(),
            channels: Vec::new(),
            lets: Vec::new(),
            masks: Vec::new(),
            states: Vec::new(),
            layers: Vec::new(),
            sims: Vec::new(),
            fns: Vec::new(),
            span: start,
        };

        loop {
            self.skip_newlines();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error(start, "unclosed `effect` block", "add a closing `}`");
                break;
            }
            let before = self.at;
            self.effect_item(&mut e);
            if self.at == before {
                self.bump();
            }
        }

        e.span = start.merge(self.span());
        Some(e)
    }

    fn effect_item(&mut self, e: &mut Effect) {
        let span = self.span();
        let keyword = match self.peek().clone() {
            Tok::Ident(k) => k,
            other => {
                self.error(
                    span,
                    alloc::format!("expected an effect item, found {}", other.describe()),
                    "inside an effect you can write `param`, `channel`, `let`, `mask`, `state`, `layer`, `sim` or `fn`",
                );
                self.recover_to_statement_end();
                return;
            }
        };
        self.bump();
        match keyword.as_str() {
            "version" => {
                e.version = self.integer("the effect version");
                self.end_of_statement();
            }
            "author" => {
                e.author = self.string("the author name");
                self.end_of_statement();
            }
            "stdlib" => {
                e.stdlib = self.integer("the stdlib version");
                self.end_of_statement();
            }
            "fps" => {
                e.fps = self.integer("the preferred frame rate");
                self.end_of_statement();
            }
            "requires" => {
                loop {
                    let cspan = self.span();
                    match self.ident("a capability") {
                        Some(name) => match Cap::from_str(&name) {
                            Some(c) => e.requires.push(c),
                            None => self.error(
                                cspan,
                                alloc::format!("unknown capability `{name}`"),
                                "capabilities are mapped, rough, rgbw, cct, audio, imu, grid and input",
                            ),
                        },
                        None => break,
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.end_of_statement();
            }
            "budget" => {
                if let Some(instructions) = self.integer("the budget") {
                    let on = match self.peek().clone() {
                        Tok::Ident(k) if k == "on" => {
                            self.bump();
                            self.ident("a device class")
                        }
                        _ => {
                            let s = self.span();
                            self.error(
                                s,
                                "expected `on` after a budget",
                                "write `budget 900 on esp32c3`",
                            );
                            None
                        }
                    };
                    if let Some(device_class) = on {
                        e.budgets.push(BudgetClaim {
                            instructions,
                            device_class,
                            span,
                        });
                    }
                }
                self.end_of_statement();
            }
            "param" => {
                if let Some(p) = self.param(span) {
                    e.params.push(p);
                }
            }
            "channel" => {
                if let Some(c) = self.channel(span) {
                    e.channels.push(c);
                }
            }
            "let" => {
                if let Some(b) = self.binding(span) {
                    e.lets.push(b);
                }
            }
            "mask" => {
                if let Some(b) = self.binding(span) {
                    e.masks.push(b);
                }
            }
            "state" => {
                if let Some(s) = self.state(span) {
                    e.states.push(s);
                }
            }
            "layer" => {
                if let Some(l) = self.layer(span) {
                    e.layers.push(l);
                }
            }
            "sim" => {
                if let Some(s) = self.sim(span) {
                    e.sims.push(s);
                }
            }
            "fn" => {
                self.at -= 1; // fn_decl expects to see the keyword
                if let Some(f) = self.fn_decl() {
                    e.fns.push(f);
                }
            }
            other => {
                self.error(
                    span,
                    alloc::format!("unknown effect item `{other}`"),
                    "inside an effect you can write `version`, `author`, `stdlib`, `requires`, `fps`, `budget`, `param`, `channel`, `let`, `mask`, `state`, `layer`, `sim` or `fn`",
                );
                self.recover_to_statement_end();
            }
        }
    }

    fn param(&mut self, start: Span) -> Option<Param> {
        let name = self.ident("a parameter name")?;
        self.expect(&Tok::Colon);
        let ty = self.ty()?;
        self.expect(&Tok::Assign);
        let default = self.expr()?;

        let mut p = Param {
            name,
            ty,
            default,
            range: None,
            unit: None,
            step: None,
            label: None,
            span: start,
        };

        // Modifiers, in any order.
        loop {
            let mspan = self.span();
            let word = match self.peek().clone() {
                Tok::Ident(w) => w,
                _ => break,
            };
            match word.as_str() {
                "range" => {
                    self.bump();
                    let lo = self.expr()?;
                    self.expect(&Tok::DotDot);
                    let hi = self.expr()?;
                    p.range = Some((lo, hi));
                }
                "unit" => {
                    self.bump();
                    let uname = self.ident("a unit")?;
                    match Unit::from_str(&uname) {
                        Some(u) => p.unit = Some(u),
                        None => self.error(
                            mspan,
                            alloc::format!("unknown unit `{uname}`"),
                            "units are m, s, ms, deg, rad, hz and %",
                        ),
                    }
                }
                "step" => {
                    self.bump();
                    p.step = Some(self.expr()?);
                }
                "label" => {
                    self.bump();
                    p.label = self.string("a label");
                }
                _ => break,
            }
        }

        p.span = start.merge(self.span());
        self.end_of_statement();
        Some(p)
    }

    fn channel(&mut self, start: Span) -> Option<Channel> {
        let name = self.ident("a channel name")?;
        self.expect(&Tok::Colon);
        let tspan = self.span();
        let tyname = self.ident("a channel type")?;
        let ty = match tyname.as_str() {
            "audio_bands" => ChanType::AudioBands,
            "audio_beat" => ChanType::AudioBeat,
            "value" => ChanType::Value,
            "vec3" => ChanType::Vec3,
            "sim" | "sensor" => {
                self.expect(&Tok::Lt);
                let inner = self.ident("a name")?;
                self.expect(&Tok::Gt);
                if tyname == "sim" {
                    ChanType::Sim(inner)
                } else {
                    ChanType::Sensor(inner)
                }
            }
            "text" => {
                let max = if self.eat(&Tok::LParen) {
                    let n = self.integer("a byte length")?;
                    self.expect(&Tok::RParen);
                    n
                } else {
                    64
                };
                ChanType::Text(max)
            }
            other => {
                self.error(
                    tspan,
                    alloc::format!("unknown channel type `{other}`"),
                    "channel types are audio_bands, audio_beat, sim<..>, sensor<..>, value, vec3 and text",
                );
                return None;
            }
        };

        let mut c = Channel {
            name,
            ty,
            hold_ms: None,
            default: None,
            span: start,
        };
        while let Tok::Ident(word) = self.peek().clone() {
            match word.as_str() {
                "hold" => {
                    self.bump();
                    c.hold_ms = self.integer("a hold time in milliseconds");
                }
                "default" => {
                    self.bump();
                    c.default = Some(self.expr()?);
                }
                _ => break,
            }
        }
        c.span = start.merge(self.span());
        self.end_of_statement();
        Some(c)
    }

    fn binding(&mut self, start: Span) -> Option<Binding> {
        let name = self.ident("a name")?;
        self.expect(&Tok::Assign);
        let value = self.expr()?;
        let span = start.merge(self.span());
        self.end_of_statement();
        Some(Binding { name, value, span })
    }

    fn state(&mut self, start: Span) -> Option<StateDecl> {
        let name = self.ident("a state name")?;
        self.expect(&Tok::Colon);
        let ty = self.ty()?;
        self.expect(&Tok::Assign);
        let init = self.expr()?;
        let span = start.merge(self.span());
        self.end_of_statement();
        Some(StateDecl {
            name,
            ty,
            init,
            span,
        })
    }

    fn layer(&mut self, start: Span) -> Option<Layer> {
        let name = self.ident("a layer name")?;
        let mut l = Layer {
            name,
            mask: None,
            blend: Blend::Normal,
            opacity: None,
            lets: Vec::new(),
            assigns: Vec::new(),
            span: start,
        };

        // Modifiers before the block.
        loop {
            let mspan = self.span();
            let word = match self.peek().clone() {
                Tok::Ident(w) => w,
                _ => break,
            };
            match word.as_str() {
                "mask" => {
                    self.bump();
                    self.expect(&Tok::LParen);
                    l.mask = self.ident("a mask name");
                    self.expect(&Tok::RParen);
                }
                "blend" => {
                    self.bump();
                    let bname = self.ident("a blend mode")?;
                    match Blend::from_str(&bname) {
                        Some(b) => l.blend = b,
                        None => self.error(
                            mspan,
                            alloc::format!("unknown blend mode `{bname}`"),
                            "blend modes are normal, add, multiply, screen, overlay, max, min and difference",
                        ),
                    }
                }
                "opacity" => {
                    self.bump();
                    l.opacity = Some(self.expr()?);
                }
                _ => break,
            }
        }

        self.expect(&Tok::LBrace);
        loop {
            self.skip_newlines();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error(start, "unclosed `layer` block", "add a closing `}`");
                break;
            }
            let ispan = self.span();
            let before = self.at;
            match self.peek().clone() {
                Tok::Ident(k) if k == "let" => {
                    self.bump();
                    if let Some(b) = self.binding(ispan) {
                        l.lets.push(b);
                    }
                }
                Tok::Ident(_) => {
                    if let Some(a) = self.assign(ispan) {
                        l.assigns.push(a);
                    }
                }
                other => {
                    self.error(
                        ispan,
                        alloc::format!(
                            "expected an assignment or `let`, found {}",
                            other.describe()
                        ),
                        "a layer contains `let` bindings and assignments like `color = ...`",
                    );
                    self.recover_to_statement_end();
                }
            }
            if self.at == before {
                self.bump();
            }
        }

        l.span = start.merge(self.span());
        Some(l)
    }

    fn assign(&mut self, start: Span) -> Option<Assign> {
        let target = self.ident("an assignment target")?;
        let field = if self.eat(&Tok::Dot) {
            Some(self.ident("a field name")?)
        } else {
            None
        };
        self.expect(&Tok::Assign);
        let value = self.expr()?;
        let span = start.merge(self.span());
        self.end_of_statement();
        Some(Assign {
            target,
            field,
            value,
            span,
        })
    }

    fn sim(&mut self, start: Span) -> Option<Sim> {
        let name = self.ident("a sim name")?;
        self.expect(&Tok::LParen);
        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                let aname = self.ident("an argument name")?;
                self.expect(&Tok::Assign);
                let value = self.expr()?;
                args.push((aname, value));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen);
        }
        let body = self.sim_block(start)?;
        Some(Sim {
            name,
            args,
            body,
            span: start.merge(self.span()),
        })
    }

    fn sim_block(&mut self, owner: Span) -> Option<Vec<SimStmt>> {
        self.expect(&Tok::LBrace);
        let mut body = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error(owner, "unclosed block", "add a closing `}`");
                break;
            }
            let before = self.at;
            if let Some(s) = self.sim_stmt() {
                body.push(s);
            }
            if self.at == before {
                self.bump();
            }
        }
        Some(body)
    }

    fn sim_stmt(&mut self) -> Option<SimStmt> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Ident(k) if k == "let" => {
                self.bump();
                self.binding(span).map(SimStmt::Let)
            }
            Tok::Ident(k) if k == "if" => {
                self.bump();
                let cond = self.expr()?;
                let then = self.sim_block(span)?;
                let otherwise = match self.peek().clone() {
                    Tok::Ident(k) if k == "else" => {
                        self.bump();
                        self.sim_block(span)?
                    }
                    _ => Vec::new(),
                };
                self.end_of_statement();
                Some(SimStmt::If {
                    cond,
                    then,
                    otherwise,
                    span: span.merge(self.span()),
                })
            }
            Tok::Ident(k) if k == "foreach" => {
                self.bump();
                let binding = self.ident("a loop variable")?;
                match self.peek().clone() {
                    Tok::Ident(k) if k == "in" => {
                        self.bump();
                    }
                    _ => {
                        let s = self.span();
                        self.error(s, "expected `in`", "write `foreach p in particles { ... }`");
                    }
                }
                let over = self.ident("an array name")?;
                let body = self.sim_block(span)?;
                self.end_of_statement();
                Some(SimStmt::ForEach {
                    binding,
                    over,
                    body,
                    span: span.merge(self.span()),
                })
            }
            Tok::Ident(_) => self.assign(span).map(SimStmt::Assign),
            other => {
                self.error(
                    span,
                    alloc::format!("expected a statement, found {}", other.describe()),
                    "a sim contains `let`, assignments, `if` and `foreach`",
                );
                self.recover_to_statement_end();
                None
            }
        }
    }

    fn fn_decl(&mut self) -> Option<FnDecl> {
        let start = self.span();
        self.bump(); // `fn`
        let name = self.ident("a function name")?;
        self.expect(&Tok::LParen);
        let mut params = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                let pname = self.ident("a parameter name")?;
                self.expect(&Tok::Colon);
                let ty = self.ty()?;
                params.push((pname, ty));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen);
        }
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.ty()?)
        } else {
            None
        };
        self.expect(&Tok::LBrace);

        let mut lets = Vec::new();
        let mut body = None;
        loop {
            self.skip_newlines();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error(start, "unclosed `fn` block", "add a closing `}`");
                break;
            }
            let span = self.span();
            let before = self.at;
            match self.peek().clone() {
                Tok::Ident(k) if k == "let" => {
                    self.bump();
                    if let Some(b) = self.binding(span) {
                        lets.push(b);
                    }
                }
                Tok::Ident(k) if k == "return" => {
                    self.bump();
                    body = self.expr();
                    self.end_of_statement();
                }
                other => {
                    self.error(
                        span,
                        alloc::format!("expected `let` or `return`, found {}", other.describe()),
                        "a function body is a sequence of `let` bindings ending in `return`",
                    );
                    self.recover_to_statement_end();
                }
            }
            if self.at == before {
                self.bump();
            }
        }

        let body = match body {
            Some(b) => b,
            None => {
                self.error(
                    start,
                    "function has no `return`",
                    "a function body ends with `return <expression>`",
                );
                return None;
            }
        };

        Some(FnDecl {
            name,
            params,
            ret,
            lets,
            body,
            span: start.merge(self.span()),
        })
    }

    // ---- palette and curve ------------------------------------------------

    fn palette(&mut self) -> Option<Palette> {
        let start = self.span();
        self.bump(); // `palette`
        let name = self.ident("a palette name")?;
        self.expect(&Tok::LBrace);

        let mut p = Palette {
            name,
            space: ColorSpace::Oklab,
            stops: Vec::new(),
            span: start,
        };

        loop {
            self.skip_newlines();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error(start, "unclosed `palette` block", "add a closing `}`");
                break;
            }
            let span = self.span();
            let before = self.at;
            match self.peek().clone() {
                Tok::Ident(k) if k == "space" => {
                    self.bump();
                    let sname = self.ident("a colour space")?;
                    match ColorSpace::from_str(&sname) {
                        Some(s) => p.space = s,
                        None => self.error(
                            span,
                            alloc::format!("unknown colour space `{sname}`"),
                            "colour spaces are oklab, oklch, hsv and linear_rgb",
                        ),
                    }
                    self.end_of_statement();
                }
                Tok::Number { text, .. } => {
                    self.bump();
                    let position = text.parse::<f64>().unwrap_or(0.0);
                    let color = self.expr()?;
                    p.stops.push(Stop {
                        position,
                        color,
                        span: span.merge(self.span()),
                    });
                    self.end_of_statement();
                }
                other => {
                    self.error(
                        span,
                        alloc::format!(
                            "expected a stop position or `space`, found {}",
                            other.describe()
                        ),
                        "a palette contains an optional `space` and stops like `0.5 #ff8000`",
                    );
                    self.recover_to_statement_end();
                }
            }
            if self.at == before {
                self.bump();
            }
        }

        p.span = start.merge(self.span());
        Some(p)
    }

    fn curve(&mut self) -> Option<Curve> {
        let start = self.span();
        self.bump(); // `curve`
        let name = self.ident("a curve name")?;
        self.expect(&Tok::LBrace);

        let mut points = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error(start, "unclosed `curve` block", "add a closing `}`");
                break;
            }
            let span = self.span();
            let before = self.at;
            let x = match self.peek().clone() {
                Tok::Number { text, .. } => {
                    self.bump();
                    text.parse::<f64>().unwrap_or(0.0)
                }
                other => {
                    self.error(
                        span,
                        alloc::format!("expected a number, found {}", other.describe()),
                        "a curve is a list of `x y` pairs, one per line",
                    );
                    self.recover_to_statement_end();
                    if self.at == before {
                        self.bump();
                    }
                    continue;
                }
            };
            let y = match self.peek().clone() {
                Tok::Number { text, .. } => {
                    self.bump();
                    text.parse::<f64>().unwrap_or(0.0)
                }
                other => {
                    let s = self.span();
                    self.error(
                        s,
                        alloc::format!("expected a second number, found {}", other.describe()),
                        "each curve line is an `x y` pair",
                    );
                    self.recover_to_statement_end();
                    continue;
                }
            };
            points.push((x, y));
            self.end_of_statement();
            if self.at == before {
                self.bump();
            }
        }

        Some(Curve {
            name,
            points,
            span: start.merge(self.span()),
        })
    }

    fn ty(&mut self) -> Option<Type> {
        let span = self.span();
        let name = self.ident("a type")?;
        match Type::from_str(&name) {
            Some(t) => Some(t),
            None => {
                self.error(
                    span,
                    alloc::format!("unknown type `{name}`"),
                    "types are float, int, bool, angle, vec2, vec3, color, palette and curve",
                );
                None
            }
        }
    }

    // ---- expressions ------------------------------------------------------

    fn expr(&mut self) -> Option<Expr> {
        self.expr_bp(0)
    }

    /// Precedence climbing.
    fn expr_bp(&mut self, min_bp: u8) -> Option<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                Tok::EqEq => BinOp::Eq,
                Tok::Ne => BinOp::Ne,
                Tok::AndAnd => BinOp::And,
                Tok::OrOr => BinOp::Or,
                _ => break,
            };
            let bp = op.precedence();
            if bp <= min_bp {
                break;
            }
            self.bump();
            let rhs = self.expr_bp(bp)?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr::new(
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Some(lhs)
    }

    fn unary(&mut self) -> Option<Expr> {
        let span = self.span();
        let op = match self.peek() {
            Tok::Minus => Some(UnOp::Neg),
            Tok::Bang => Some(UnOp::Not),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let operand = self.unary()?;
            let full = span.merge(operand.span);
            return Some(Expr::new(
                ExprKind::Unary {
                    op,
                    operand: Box::new(operand),
                },
                full,
            ));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Option<Expr> {
        let mut e = self.primary()?;
        loop {
            if !self.eat(&Tok::Dot) {
                break;
            }
            let fspan = self.span();
            let field = self.ident("a field name")?;
            if self.eat(&Tok::LParen) {
                let args = self.call_args()?;
                let span = e.span.merge(self.span());
                e = Expr::new(
                    ExprKind::MethodCall {
                        base: Box::new(e),
                        method: field,
                        args,
                    },
                    span,
                );
            } else {
                let span = e.span.merge(fspan);
                e = Expr::new(
                    ExprKind::Field {
                        base: Box::new(e),
                        field,
                    },
                    span,
                );
            }
        }
        Some(e)
    }

    fn call_args(&mut self) -> Option<Vec<Expr>> {
        let mut args = Vec::new();
        if self.eat(&Tok::RParen) {
            return Some(args);
        }
        loop {
            args.push(self.expr()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen);
        Some(args)
    }

    fn primary(&mut self) -> Option<Expr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Number { text, unit } => {
                self.bump();
                let raw: f64 = match text.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        self.error(span, "malformed number", "write a decimal like `1.5`");
                        return None;
                    }
                };
                // Units convert here, once, so nothing downstream has to
                // remember. `90deg` is radians from this point on.
                let value = match unit {
                    Some(Unit::Deg) => raw * core::f64::consts::PI / 180.0,
                    Some(Unit::Ms) => raw / 1000.0,
                    Some(Unit::Percent) => raw / 100.0,
                    _ => raw,
                };
                Some(Expr::new(ExprKind::Number { value, unit }, span))
            }
            Tok::HexColor(rgba) => {
                self.bump();
                // sRGB to linear. An effect never sees a gamma-encoded value.
                let lin = |c: u8| srgb_to_linear(c as f64 / 255.0);
                Some(Expr::new(
                    ExprKind::Color([
                        lin(rgba[0]),
                        lin(rgba[1]),
                        lin(rgba[2]),
                        rgba[3] as f64 / 255.0,
                    ]),
                    span,
                ))
            }
            Tok::Str(s) => {
                self.bump();
                Some(Expr::new(ExprKind::Str(s), span))
            }
            Tok::Ident(name) => {
                self.bump();
                if self.eat(&Tok::LParen) {
                    let args = self.call_args()?;
                    Some(Expr::new(
                        ExprKind::Call { callee: name, args },
                        span.merge(self.span()),
                    ))
                } else {
                    Some(Expr::new(ExprKind::Ident(name), span))
                }
            }
            Tok::LParen => {
                self.bump();
                let inner = self.expr()?;
                self.expect(&Tok::RParen);
                Some(inner)
            }
            other => {
                self.error(
                    span,
                    alloc::format!("expected an expression, found {}", other.describe()),
                    "an expression is a number, a name, a call, or an operation on them",
                );
                None
            }
        }
    }
}

/// sRGB transfer function, inverted.
///
/// Applied at parse time so `color` is linear everywhere downstream. Blending in
/// linear and encoding once at the end is the difference between fades that look
/// right and fades that look cheap.
pub fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        srgb_pow((c + 0.055) / 1.055, 2.4)
    }
}

/// `x^y` for the small, positive domain the colour transfer needs.
///
/// A local implementation rather than a dependency: this crate is `no_std` and
/// pulling in a maths library to convert eight-bit colour literals would reach
/// every device in the mesh.
pub(crate) fn srgb_pow(x: f64, y: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    // exp(y * ln x), by series on a range reduced to [1, 2).
    let mut exponent = 0i32;
    let mut m = x;
    while m >= 2.0 {
        m /= 2.0;
        exponent += 1;
    }
    while m < 1.0 {
        m *= 2.0;
        exponent -= 1;
    }
    let ln = ln_1to2(m) + (exponent as f64) * core::f64::consts::LN_2;
    exp_series(y * ln)
}

/// Natural log on `[1, 2)`, by the atanh series.
fn ln_1to2(m: f64) -> f64 {
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    let mut term = z;
    let mut sum = 0.0;
    for k in 0..12 {
        sum += term / (2 * k + 1) as f64;
        term *= z2;
    }
    2.0 * sum
}

fn exp_series(x: f64) -> f64 {
    // Range-reduce by powers of two, then Taylor.
    let n = if x < 0.0 {
        (x - 0.5) as i32
    } else {
        (x + 0.5) as i32
    };
    let r = x - n as f64;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..18 {
        term *= r / k as f64;
        sum += term;
    }
    let mut out = sum;
    let mut i = 0;
    while i < n.abs() {
        if n > 0 {
            out *= core::f64::consts::E;
        } else {
            out /= core::f64::consts::E;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_power_function_guards_its_domain() {
        // `srgb_pow` is only ever called with a positive base, but the guard is
        // what keeps a future caller out of `ln(0)`. Nothing above can reach it,
        // so pin it here.
        assert_eq!(srgb_pow(0.0, 2.4), 0.0);
        assert_eq!(srgb_pow(-1.0, 2.4), 0.0);
    }

    #[test]
    fn the_local_power_function_agrees_with_known_values() {
        // No dependency, no libm: if this drifts, every hex colour in every
        // effect shifts with it.
        assert!((srgb_pow(1.0, 2.4) - 1.0).abs() < 1e-9);
        assert!((srgb_pow(4.0, 0.5) - 2.0).abs() < 1e-6);
        assert!((srgb_pow(0.5, 2.0) - 0.25).abs() < 1e-6);
        assert!((srgb_pow(0.1, 2.4) - 0.003_981_071_705).abs() < 1e-9);
    }

    #[test]
    fn an_example_name_is_an_identifier_a_person_could_have_typed() {
        // Every call site passes an article phrase, because that is what reads
        // correctly in "expected {what}, found ...". The help line needs the
        // same thing as an identifier instead.
        assert_eq!(Parser::example_name("a parameter name"), "my_parameter");
        assert_eq!(Parser::example_name("an argument name"), "my_argument");
        assert_eq!(Parser::example_name("a capability"), "my_capability");
        assert_eq!(Parser::example_name("a device class"), "my_device_class");
        assert_eq!(
            Parser::example_name("an assignment target"),
            "my_assignment_target"
        );
        assert_eq!(Parser::example_name("a blend mode"), "my_blend_mode");
    }

    #[test]
    fn a_bare_name_does_not_collapse_to_an_empty_example() {
        // "a name" trims to "name", not to "", or the help line would read
        // "must be a name like `my_`".
        assert_eq!(Parser::example_name("a name"), "my_name");
    }

    #[test]
    fn every_example_name_is_a_valid_identifier() {
        // The help line claims the example is a name; if it is not lexable as
        // one, the suggestion is worse than none.
        for what in [
            "a capability",
            "a device class",
            "a parameter name",
            "a unit",
            "a channel name",
            "a channel type",
            "a name",
            "a state name",
            "a layer name",
            "a mask name",
            "a blend mode",
            "an assignment target",
            "a field name",
            "a sim name",
            "an argument name",
            "a loop variable",
            "an array name",
            "a function name",
        ] {
            let example = Parser::example_name(what);
            assert!(!example.is_empty(), "{what} produced an empty example");
            assert!(
                example
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{what} produced `{example}`, which is not an identifier"
            );
            let (toks, _, errs) = lex_with_comments(&example);
            assert!(
                errs.is_empty(),
                "{what} produced `{example}`, which does not lex"
            );
            assert!(
                matches!(&toks[0].tok, Tok::Ident(n) if *n == example),
                "{what} produced `{example}`, which does not lex as a single identifier"
            );
        }
    }
}
