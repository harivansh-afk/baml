//! One resumable implementation per admitted MIR body. Locals are typed Rust
//! fields, call results enter through an explicit continuation, and source-site
//! IDs remain independent of the state-machine block number.

use std::{collections::HashMap, fmt::Write as _};

use baml_base::Span;
use baml_compiler2_hir_ty::extern_loc::FunctionRef;
use baml_compiler2_mir::{
    BinOp, BlockId, Constant, IndexKind, Local, MirFunctionBody, Operand, Place, Rvalue,
    ShortCircuitKind, StatementKind, SwitchKey, Terminator, UnaryOp,
};
use baml_type::Int63;
use bex_vm_types::compiled::{CompiledSite, SiteKind};

use crate::{NativeType, Rejection, direct_callee};

type Result<T> = std::result::Result<T, Rejection>;

pub(crate) struct ResolvedCalls<'db> {
    pub(crate) ids: HashMap<FunctionRef<'db>, usize>,
    pub(crate) slots: Vec<usize>,
}

/// Facts shared by both entry shapes. This is an emission plan over the same
/// checked MIR, not another lowered representation of the program.
pub(crate) struct PreparedFunction {
    blocks: Vec<Option<PreparedBlock>>,
    sites: Vec<CompiledSite>,
}

struct PreparedBlock {
    initialized: Vec<bool>,
    statements: Vec<usize>,
    terminator: usize,
}

pub(crate) fn prepare<'db>(
    candidate: &crate::Candidate<'db>,
    calls: &ResolvedCalls<'db>,
    source: &str,
) -> Result<PreparedFunction> {
    let crate::Candidate {
        arity,
        body,
        types,
        span: fallback_span,
    } = candidate;
    let (arity, fallback_span) = (*arity, *fallback_span);
    // Unsupported control flow is an admission decision. Structural failures
    // in the supported CFG are compiler errors and must not become fallback.
    for block in &body.blocks {
        if let Some(term) = &block.terminator {
            match term {
                Terminator::Goto { .. }
                | Terminator::Branch { .. }
                | Terminator::Switch { .. }
                | Terminator::Return
                | Terminator::Unreachable
                | Terminator::Call { .. }
                | Terminator::ShortCircuit { .. } => {}
                other => return Err(Rejection::unsupported(format!("terminator {other:?}"))),
            }
        }
    }
    let states = initialized_on_entry(body, arity).map_err(Rejection::invalid)?;
    let line_starts: Vec<u32> = std::iter::once(0)
        .chain(
            source
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(index, _)| u32::try_from(index + 1).expect("source offsets fit TextSize")),
        )
        .collect();
    let mut sites = Vec::new();
    // Entry includes failure before the first operation and a GC handoff just
    // after the caller pushed this frame. Both entry shapes use this same map.
    site(&mut sites, fallback_span, source, &line_starts, false)?;
    let direct = HashMap::new();
    let mut validator = Emitter {
        types,
        callees: &calls.ids,
        initialized: Vec::new(),
        direct: &direct,
        resumable: true,
    };
    let mut blocks = Vec::with_capacity(body.blocks.len());
    for (block, state) in body.blocks.iter().zip(states) {
        let Some(initialized) = state else {
            blocks.push(None);
            continue;
        };
        validator.initialized.clone_from(&initialized);
        let mut statements = Vec::with_capacity(block.statements.len());
        for statement in &block.statements {
            statements.push(site(
                &mut sites,
                statement.span.or(block.span).or(fallback_span),
                source,
                &line_starts,
                false,
            )?);
            // Reuse the operation emitter's type/admission rules. Only the
            // small operation fragment is discarded, not an entire Rust body.
            validator.statement(&statement.kind)?;
        }
        let term = block
            .terminator
            .as_ref()
            .expect("analysis verified terminators");
        let terminator = site(
            &mut sites,
            block.terminator_span.or(block.span).or(fallback_span),
            source,
            &line_starts,
            matches!(term, Terminator::Call { .. }),
        )?;
        validator.terminator(term, block.id)?;
        blocks.push(Some(PreparedBlock {
            initialized,
            statements,
            terminator,
        }));
    }
    Ok(PreparedFunction { blocks, sites })
}

pub(crate) fn emit<'db>(
    id: usize,
    candidate: &crate::Candidate<'db>,
    calls: &ResolvedCalls<'db>,
    prepared: &PreparedFunction,
    direct: &HashMap<FunctionRef<'db>, crate::direct::Target>,
) -> Result<String> {
    let crate::Candidate {
        arity, body, types, ..
    } = candidate;
    let arity = *arity;
    let (callees, slots) = (&calls.ids, &calls.slots);
    let mut emitter = Emitter {
        types,
        callees,
        initialized: Vec::new(),
        direct,
        resumable: true,
    };
    let mut out =
        format!("\nstruct Frame{id} {{ block: usize, site: usize, waiting: Option<usize>,\n");
    for (i, ty) in types.iter().enumerate() {
        let _ = writeln!(out, "    _{i}: {},", ty.rust());
    }
    out.push_str("}\n");
    let _ = writeln!(
        out,
        "impl RootHaver for Frame{id} {{\n    fn collect_roots(&self, roots: &mut Vec<HeapPtr>) {{"
    );
    for (i, ty) in types.iter().enumerate() {
        if *ty == NativeType::IntArray {
            let _ = writeln!(out, "        roots.extend(self._{i}.as_object_ptr());");
        }
    }
    out.push_str("    }\n    fn forward_roots(&mut self, roots: &HashMap<HeapPtr, HeapPtr>) {\n");
    for (i, ty) in types.iter().enumerate() {
        if *ty == NativeType::IntArray {
            let _ = writeln!(
                out,
                "        if let Some(pointer) = self._{i}.as_object_ptr() {{ if let Some(&moved) = roots.get(&pointer) {{ self._{i} = Value::object(moved); }} }}"
            );
        }
    }
    out.push_str("    }\n}\n");
    let _ = writeln!(
        out,
        "impl CompiledFrame for Frame{id} {{\n    fn site(&self) -> usize {{ self.site }}\n    fn resume(&mut self, input: ResumeInput, poll: &mut EarlyYieldCheck, runtime: &mut dyn CompiledRuntime) -> Result<CompiledAction, VmRustFnError> {{"
    );
    out.push_str("        match (self.waiting.take(), input) {\n            (None, ResumeInput::Continue) => {},\n            (Some(call), ResumeInput::Returned(_value)) => match call {\n");
    for (block, plan) in body.blocks.iter().zip(&prepared.blocks) {
        if plan.is_none() {
            continue;
        }
        if let Some(Terminator::Call {
            destination,
            target,
            ..
        }) = &block.terminator
        {
            let local = local_place(destination)?;
            let _ = writeln!(
                out,
                "                {} => {{ self._{} = {}; self.block = {}; }},",
                block.id.0,
                local.0,
                types[local.0].read("_value"),
                target.0
            );
        }
    }
    out.push_str("                _ => return Err(VmInternalError::InvalidCompiledCode { message: \"unknown compiled continuation\".into() }.into()),\n            },\n            _ => return Err(VmInternalError::InvalidCompiledCode { message: \"unexpected compiled resume input\".into() }.into()),\n        }\n        loop {\n            if poll.tick() && runtime.poll_for_yield(poll) { return Ok(CompiledAction::Yield); }\n            match self.block {\n");
    for (block, plan) in body.blocks.iter().zip(&prepared.blocks) {
        let Some(plan) = plan else {
            continue;
        };
        emitter.initialized.clone_from(&plan.initialized);
        let _ = writeln!(out, "                {} => {{", block.id.0);
        for (statement, site) in block.statements.iter().zip(&plan.statements) {
            let _ = writeln!(out, "                    self.site = {site};");
            if let Some(line) = emitter.statement(&statement.kind)? {
                let _ = writeln!(out, "                    {line}");
            }
        }
        let term = block
            .terminator
            .as_ref()
            .expect("analysis verified terminators");
        let site = plan.terminator;
        let _ = writeln!(out, "                    self.site = {site};");
        for line in emitter.terminator(term, block.id)? {
            let _ = writeln!(out, "                    {line}");
        }
        out.push_str("                }\n");
    }
    out.push_str("                _ => return Err(VmInternalError::InvalidCompiledCode { message: \"unknown compiled block\".into() }.into()),\n            }\n        }\n    }\n}\n");
    let _ = writeln!(
        out,
        "fn create_{id}(args: &[Value], runtime: &dyn CompiledHeap) -> Result<Box<dyn CompiledFrame>, VmInternalError> {{\n    if args.len() != {arity} {{ return Err(VmInternalError::InvalidArgumentCount {{ expected: {arity}, got: args.len() }}); }}\n    Ok(Box::new(Frame{id} {{ block: {}, site: 0, waiting: None,",
        body.entry.0
    );
    for (i, ty) in types.iter().enumerate() {
        let value = if (1..=arity).contains(&i) {
            ty.read(&format!("args[{}]", i - 1))
        } else {
            ty.zero().into()
        };
        let _ = writeln!(out, "        _{i}: {value},");
    }
    out.push_str("    }))\n}\n");
    let _ = writeln!(
        out,
        "static CODE_{id}: CompiledCode = CompiledCode {{ create: create_{id}, calls: &{slots:?}, sites: &["
    );
    for entry in &prepared.sites {
        let CompiledSite {
            file_id,
            start,
            end,
            line,
            kind,
        } = entry;
        let _ = writeln!(
            out,
            "    CompiledSite {{ file_id: {file_id}, start: {start}, end: {end}, line: {line}, kind: SiteKind::{kind:?} }},"
        );
    }
    out.push_str("] };\n");
    Ok(out)
}

/// Emit ordinary typed Rust locals and calls for a proven finite scalar region.
/// Statement semantics and call lowering are shared with the resumable emitter.
pub(crate) fn emit_direct<'db>(
    target: &crate::direct::Target,
    candidate: &crate::Candidate<'db>,
    calls: &ResolvedCalls<'db>,
    prepared: &PreparedFunction,
    direct: &HashMap<FunctionRef<'db>, crate::direct::Target>,
) -> Result<String> {
    let id = target.object;
    let types = &candidate.types;
    let result_ty = types[0];
    let mut emitter = Emitter {
        types,
        callees: &calls.ids,
        initialized: Vec::new(),
        direct,
        resumable: false,
    };
    let parameters = target
        .parameters
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("mut _{}: {}", i + 1, ty.rust()))
        .collect::<Vec<_>>()
        .join(", ");
    let arguments = target
        .parameters
        .iter()
        .enumerate()
        .map(|(i, ty)| ty.value(&format!("_{}", i + 1)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "\n#[allow(unused_mut, unused_variables, unused_assignments)]\nfn direct_{id}(runtime: &mut dyn CompiledRuntime, caller_site: usize, {parameters}) -> Result<{}, VmRustFnError> {{\n    runtime.enter_direct(caller_site, {}, &CODE_{id}, &[{arguments}])?;\n",
        result_ty.rust(),
        target.global,
    );
    for (i, ty) in types.iter().enumerate() {
        if !(1..=candidate.arity).contains(&i) {
            let _ = writeln!(out, "    let mut _{i}: {} = {};", ty.rust(), ty.zero());
        }
    }
    let _ = writeln!(
        out,
        "    let mut block = {};\n    let mut site = 0;\n    let result = (|| -> Result<{}, VmRustFnError> {{\n        loop {{ match block {{",
        candidate.body.entry.0,
        result_ty.rust()
    );
    for (block, plan) in candidate.body.blocks.iter().zip(&prepared.blocks) {
        let Some(plan) = plan else {
            continue;
        };
        emitter.initialized.clone_from(&plan.initialized);
        let _ = writeln!(out, "            {} => {{", block.id.0);
        for (statement, site) in block.statements.iter().zip(&plan.statements) {
            let _ = writeln!(out, "                site = {site};");
            if let Some(line) = emitter.statement(&statement.kind)? {
                let _ = writeln!(out, "                {line}");
            }
        }
        let _ = writeln!(out, "                site = {};", plan.terminator);
        for line in emitter.terminator(
            block.terminator.as_ref().expect("admitted terminator"),
            block.id,
        )? {
            let _ = writeln!(out, "                {line}");
        }
        out.push_str("            }\n");
    }
    out.push_str("            _ => return Err(VmInternalError::InvalidCompiledCode { message: \"unknown direct block\".into() }.into()),\n        }}\n    })();\n    match result {\n");
    let _ = writeln!(
        out,
        "        Ok(value) => {{ runtime.return_direct({})?; Ok(value) }},",
        result_ty.value("value")
    );
    let _ = writeln!(
        out,
        "        Err(error) => {{ runtime.fail_direct(&CODE_{id}, site); Err(error) }},\n    }}\n}}"
    );
    Ok(out)
}

fn site(
    sites: &mut Vec<CompiledSite>,
    span: Option<Span>,
    source: &str,
    line_starts: &[u32],
    call: bool,
) -> Result<usize> {
    let span = span.ok_or_else(|| Rejection::unsupported("missing source location"))?;
    let start = usize::from(span.range.start());
    let end = usize::from(span.range.end());
    source
        .get(..start)
        .ok_or_else(|| Rejection::invalid("source span starts outside its file"))?;
    if source.get(start..end).is_none() {
        return Err(Rejection::invalid("source span ends outside its file"));
    }
    let line = span.start_line(line_starts);
    let kind = if call {
        SiteKind::Call
    } else {
        SiteKind::Operation
    };
    let id = sites.len();
    sites.push(CompiledSite {
        file_id: span.file_id.as_u32(),
        start: span.range.start().into(),
        end: span.range.end().into(),
        line: u32::try_from(line).expect("source lines fit TextSize"),
        kind,
    });
    Ok(id)
}

struct Emitter<'a, 'db> {
    types: &'a [NativeType],
    callees: &'a HashMap<FunctionRef<'db>, usize>,
    initialized: Vec<bool>,
    direct: &'a HashMap<FunctionRef<'db>, crate::direct::Target>,
    resumable: bool,
}

impl<'db> Emitter<'_, 'db> {
    fn local_name(&self, local: Local) -> String {
        format!(
            "{}_{index}",
            if self.resumable { "self." } else { "" },
            index = local.0
        )
    }
    fn block_name(&self) -> &'static str {
        if self.resumable {
            "self.block"
        } else {
            "block"
        }
    }
    fn site_name(&self) -> &'static str {
        if self.resumable { "self.site" } else { "site" }
    }

    fn statement(&mut self, kind: &StatementKind<'db>) -> Result<Option<String>> {
        match kind {
            StatementKind::Assign { destination, value } => {
                let (expr, ty) = self.rvalue(value)?;
                match destination {
                    Place::Local(local) => {
                        self.expect(*local, ty)?;
                        self.initialized[local.0] = true;
                        Ok(Some(format!("{} = {expr};", self.local_name(*local))))
                    }
                    Place::Index {
                        base,
                        index,
                        kind: IndexKind::Array,
                    } => {
                        if ty != NativeType::Int {
                            return Err(Rejection::invalid("int[] store requires int"));
                        }
                        let array = self.place_of(base, NativeType::IntArray)?;
                        let index = self.place_of(&Place::Local(*index), NativeType::Int)?;
                        // MIR evaluates the RHS before storing into its destination.
                        Ok(Some(format!(
                            "let value = {expr}; runtime.int_array_set({array}, {index}, value)?;"
                        )))
                    }
                    _ => Err(Rejection::unsupported(format!("place {destination}"))),
                }
            }
            StatementKind::Drop(place) => {
                // MIR Drop evaluates and discards a value; it does not end
                // the lifetime of the local that was read. Index reads can trap.
                let (value, _) = self.place(place)?;
                Ok(Some(format!("let _ = {value};")))
            }
            StatementKind::Nop => Ok(None),
            other => Err(Rejection::unsupported(format!("statement {other:?}"))),
        }
    }

    fn terminator(&self, term: &Terminator<'db>, block: BlockId) -> Result<Vec<String>> {
        let block_name = self.block_name();
        let line = match term {
            Terminator::Goto { target } => format!("{block_name} = {};", target.0),
            Terminator::Branch {
                condition,
                then_block,
                else_block,
            } => {
                let condition = self.operand_of(condition, NativeType::Bool)?;
                format!(
                    "{block_name} = if {condition} {{ {} }} else {{ {} }};",
                    then_block.0, else_block.0
                )
            }
            Terminator::Switch {
                discriminant,
                arms,
                otherwise,
                ..
            } => {
                let discriminant = self.operand_of(discriminant, NativeType::Int)?;
                let mut seen = Vec::new();
                let mut out = format!("{block_name} = match ({discriminant}).get() {{ ");
                for (key, target) in arms {
                    let SwitchKey::Int(key) = key else {
                        return Err(Rejection::unsupported("non-integer switch"));
                    };
                    if !seen.contains(key) {
                        seen.push(*key);
                        let _ = write!(out, "{key} => {}, ", target.0);
                    }
                }
                let _ = write!(out, "_ => {} }};", otherwise.0);
                out
            }
            Terminator::Return => {
                let ty = self.read(Local(0))?;
                if self.resumable {
                    format!(
                        "return Ok(CompiledAction::Return({}));",
                        ty.value("self._0")
                    )
                } else {
                    format!("return Ok({});", self.local_name(Local(0)))
                }
            }
            Terminator::Unreachable => "return Err(VmPanic::Unreachable.into());".into(),
            Terminator::Call {
                args,
                destination,
                target: continuation,
                ..
            } => {
                let callee = direct_callee(term).map_err(Rejection::unsupported)?;
                if let Some(target) = self.direct.get(&callee) {
                    if args.len() != target.parameters.len() {
                        return Err(Rejection::invalid("direct call signature mismatch"));
                    }
                    let args = args
                        .iter()
                        .zip(&target.parameters)
                        .map(|(arg, ty)| self.operand_of(arg, *ty))
                        .collect::<Result<Vec<_>>>()?
                        .join(", ");
                    let destination = local_place(destination)?;
                    self.expect(destination, target.result)?;
                    // Direct chains have no internal safepoints. Charge their
                    // full bounded work once at the resumable boundary, before
                    // entering so errors also retain the charge. The next block
                    // performs the existing poll after all direct frames unwind.
                    let charge = if self.resumable {
                        format!("poll.account_work({}); ", target.work)
                    } else {
                        String::new()
                    };
                    format!(
                        "{charge}{} = direct_{}(runtime, {}, {args})?; {block_name} = {};",
                        self.local_name(destination),
                        target.object,
                        self.site_name(),
                        continuation.0
                    )
                } else {
                    if !self.resumable {
                        return Err(Rejection::invalid(
                            "direct body calls a non-direct function",
                        ));
                    }
                    let target = self
                        .callees
                        .get(&callee)
                        .ok_or_else(|| Rejection::invalid("unbound direct call"))?;
                    let args = args
                        .iter()
                        .map(|arg| {
                            let (value, ty) = self.operand(arg)?;
                            Ok(ty.value(&value))
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join(", ");
                    format!(
                        "self.waiting = Some({}); return Ok(CompiledAction::Call {{ target: {target}, args: vec![{args}] }});",
                        block.0
                    )
                }
            }
            Terminator::ShortCircuit {
                operand,
                kind,
                destination,
                eval_rhs,
                join,
            } => {
                let value = self.operand_of(operand, NativeType::Bool)?;
                let dest = local_place(destination)?;
                self.expect(dest, NativeType::Bool)?;
                let short = format!(
                    "{} = {value}; {block_name} = {};",
                    self.local_name(dest),
                    join.0
                );
                let long = format!("{block_name} = {};", eval_rhs.0);
                match kind {
                    ShortCircuitKind::And => format!("if {value} {{ {long} }} else {{ {short} }}"),
                    ShortCircuitKind::Or => format!("if {value} {{ {short} }} else {{ {long} }}"),
                    ShortCircuitKind::Coalesce => return Err(Rejection::unsupported("coalescing")),
                }
            }
            other => return Err(Rejection::unsupported(format!("terminator {other:?}"))),
        };
        Ok(vec![line])
    }

    fn rvalue(&self, value: &Rvalue<'db>) -> Result<(String, NativeType)> {
        use NativeType::{Bool, Int, IntArray};
        match value {
            Rvalue::Use(operand) => self.operand(operand),
            Rvalue::Array(baml_type::TyTemplate::Int, values) => {
                let values = values
                    .iter()
                    .map(|value| self.operand_of(value, Int))
                    .collect::<Result<Vec<_>>>()?
                    .join(", ");
                Ok((
                    format!("runtime.alloc_int_array(vec![{values}], poll)"),
                    IntArray,
                ))
            }
            Rvalue::Len(place) => {
                let array = self.place_of(place, IntArray)?;
                Ok((format!("runtime.int_array_len({array})?"), Int))
            }
            Rvalue::UnaryOp { op, operand } => {
                let (value, ty) = self.operand(operand)?;
                Ok(match (op, ty) {
                    (UnaryOp::Neg, Int) => (format!("int::neg({value})?"), Int),
                    (UnaryOp::Not, Int) => (format!("({value}).get() == 0"), Bool),
                    (UnaryOp::Not, Bool) => (format!("!{value}"), Bool),
                    (UnaryOp::Truthy, Int) => (format!("({value}).get() != 0"), Bool),
                    (UnaryOp::Truthy, Bool) => (value, Bool),
                    _ => return Err(Rejection::unsupported(format!("{op:?} on {ty:?}"))),
                })
            }
            Rvalue::BinaryOp { op, left, right } => {
                let (left, ty) = self.operand(left)?;
                let right = self.operand_of(right, ty)?;
                let checked = |name| Ok((format!("int::{name}({left}, {right})?"), Int));
                // MIR Display is diagnostic syntax, not a Rust token contract.
                let infix = |token: &str, result| Ok((format!("{left} {token} {right}"), result));
                match (op, ty) {
                    (BinOp::Add, Int) => checked("add"),
                    (BinOp::Sub, Int) => checked("sub"),
                    (BinOp::Mul, Int) => checked("mul"),
                    (BinOp::Div, Int) => checked("div"),
                    (BinOp::Mod, Int) => checked("rem"),
                    (BinOp::Shl, Int) => checked("shl"),
                    (BinOp::Shr, Int) => checked("shr"),
                    (BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, Int) => {
                        let method = match op {
                            BinOp::BitAnd => "bit_and",
                            BinOp::BitOr => "bit_or",
                            _ => "bit_xor",
                        };
                        Ok((format!("({left}).{method}({right})"), Int))
                    }
                    (BinOp::BitAnd, Bool) => infix("&", Bool),
                    (BinOp::BitOr, Bool) => infix("|", Bool),
                    (BinOp::BitXor, Bool) => infix("^", Bool),
                    (BinOp::Eq, Int | Bool) => infix("==", Bool),
                    (BinOp::Ne, Int | Bool) => infix("!=", Bool),
                    (BinOp::Lt, Int) => infix("<", Bool),
                    (BinOp::Le, Int) => infix("<=", Bool),
                    (BinOp::Gt, Int) => infix(">", Bool),
                    (BinOp::Ge, Int) => infix(">=", Bool),
                    _ => Err(Rejection::unsupported(format!("{op:?} on {ty:?}"))),
                }
            }
            other => Err(Rejection::unsupported(format!("rvalue {other:?}"))),
        }
    }

    fn operand(&self, operand: &Operand<'db>) -> Result<(String, NativeType)> {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.place(place),
            Operand::Constant(Constant::Int(n)) if Int63::new(*n).is_some() => Ok((
                format!("Int63::new({n}).expect(\"checked MIR constant\")"),
                NativeType::Int,
            )),
            Operand::Constant(Constant::Bool(b)) => Ok((b.to_string(), NativeType::Bool)),
            other @ Operand::Constant(_) => {
                Err(Rejection::unsupported(format!("operand {other:?}")))
            }
        }
    }
    fn place(&self, place: &Place) -> Result<(String, NativeType)> {
        match place {
            Place::Local(local) => Ok((self.local_name(*local), self.read(*local)?)),
            Place::Index {
                base,
                index,
                kind: IndexKind::Array,
            } => {
                let array = self.place_of(base, NativeType::IntArray)?;
                let index = self.place_of(&Place::Local(*index), NativeType::Int)?;
                Ok((
                    format!("runtime.int_array_get({array}, {index})?"),
                    NativeType::Int,
                ))
            }
            _ => Err(Rejection::unsupported(format!("place {place}"))),
        }
    }
    fn place_of(&self, place: &Place, expected: NativeType) -> Result<String> {
        let (value, ty) = self.place(place)?;
        if ty != expected {
            return Err(Rejection::invalid(format!(
                "expected {expected:?}, found {ty:?}"
            )));
        }
        Ok(value)
    }
    fn operand_of(&self, operand: &Operand<'db>, expected: NativeType) -> Result<String> {
        let (value, ty) = self.operand(operand)?;
        if ty != expected {
            return Err(Rejection::invalid(format!(
                "expected {expected:?}, found {ty:?}"
            )));
        }
        Ok(value)
    }
    fn read(&self, local: Local) -> Result<NativeType> {
        let ty = self.local_type(local)?;
        if !self.initialized[local.0] {
            return Err(Rejection::invalid(format!(
                "{local} may be read before assignment"
            )));
        }
        Ok(ty)
    }
    fn expect(&self, local: Local, ty: NativeType) -> Result<()> {
        let expected = self.local_type(local)?;
        if expected != ty {
            return Err(Rejection::invalid(format!(
                "{local} is {expected:?}, assigned {ty:?}"
            )));
        }
        Ok(())
    }
    fn local_type(&self, local: Local) -> Result<NativeType> {
        self.types
            .get(local.0)
            .copied()
            .ok_or_else(|| Rejection::invalid(format!("{local} is not declared")))
    }
}

fn local_place(place: &Place) -> Result<Local> {
    match place {
        Place::Local(local) => Ok(*local),
        other => Err(Rejection::unsupported(format!("place {other}"))),
    }
}

/// Which locals are assigned on every path into each block, or `None` for a
/// block no path reaches. Parameters are assigned on entry.
///
/// MIR's verifier runs only in debug builds. Native emission needs this check
/// in release as well: zero-initialized Rust storage must never conceal an
/// uninitialized BAML read. This subset excludes cells and handlers; its edge
/// writes must stay consistent with MIR's definite-assignment verifier.
fn initialized_on_entry(
    body: &MirFunctionBody<'_>,
    arity: usize,
) -> std::result::Result<Vec<Option<Vec<bool>>>, String> {
    let blocks = body.blocks.len();
    let mut writes = Vec::with_capacity(blocks);
    let mut edges = Vec::with_capacity(blocks);
    for (i, block) in body.blocks.iter().enumerate() {
        if block.id.0 != i {
            return Err(format!("{} is stored at position {i}", block.id));
        }
        let terminator = block
            .terminator
            .as_ref()
            .ok_or_else(|| format!("{} has no terminator", block.id))?;
        let block_edges = successors(terminator)?;
        if let Some((target, _)) = block_edges.iter().find(|(t, _)| t.0 >= blocks) {
            return Err(format!("{} jumps to missing {target}", block.id));
        }
        let block_writes = block
            .statements
            .iter()
            .filter_map(|s| match &s.kind {
                StatementKind::Assign {
                    destination: Place::Local(local),
                    ..
                } => Some(*local),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some(local) = block_writes
            .iter()
            .chain(block_edges.iter().filter_map(|(_, write)| write.as_ref()))
            .find(|l| l.0 >= body.locals.len())
        {
            return Err(format!("{} assigns undeclared {local}", block.id));
        }
        edges.push(block_edges);
        writes.push(block_writes);
    }
    if body.entry.0 >= blocks {
        return Err(format!("entry {} is missing", body.entry));
    }
    let locals = body.locals.len();
    let mut entry = vec![false; locals];
    entry[1..=arity].fill(true);
    let mut states: Vec<Option<Vec<bool>>> = vec![None; blocks];
    states[body.entry.0] = Some(entry);
    let mut work = vec![body.entry.0];
    while let Some(i) = work.pop() {
        let mut after = states[i].clone().expect("only reached blocks are queued");
        for local in &writes[i] {
            after[local.0] = true;
        }
        for (target, write) in &edges[i] {
            let mut edge = after.clone();
            if let Some(local) = write {
                edge[local.0] = true;
            }
            let merged = match &states[target.0] {
                None => edge,
                Some(old) => old.iter().zip(&edge).map(|(a, b)| *a && *b).collect(),
            };
            if states[target.0].as_ref() != Some(&merged) {
                states[target.0] = Some(merged);
                work.push(target.0);
            }
        }
    }
    Ok(states)
}

/// Each successor of a supported terminator, with the local it assigns on the
/// way there.
fn successors(
    terminator: &Terminator<'_>,
) -> std::result::Result<Vec<(BlockId, Option<Local>)>, String> {
    Ok(match terminator {
        Terminator::Goto { target } => vec![(*target, None)],
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![(*then_block, None), (*else_block, None)],
        Terminator::Switch {
            arms, otherwise, ..
        } => arms
            .iter()
            .map(|(_, target)| (*target, None))
            .chain([(*otherwise, None)])
            .collect(),
        Terminator::Return | Terminator::Unreachable => Vec::new(),
        Terminator::Call {
            destination,
            target,
            ..
        } => vec![(
            *target,
            Some(match destination {
                Place::Local(local) => *local,
                _ => return Err("non-local call destination".into()),
            }),
        )],
        Terminator::ShortCircuit {
            destination,
            eval_rhs,
            join,
            ..
        } => vec![
            (*eval_rhs, None),
            (
                *join,
                Some(match destination {
                    Place::Local(local) => *local,
                    _ => return Err("non-local call destination".into()),
                }),
            ),
        ],
        other => return Err(format!("unsupported terminator {other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use baml_compiler2_mir::{BasicBlock, LocalDecl, RuntimeTy, Statement};

    use super::*;

    #[test]
    fn short_circuit_assignment_exists_only_on_the_short_edge() {
        let mut entry = BasicBlock::new(BlockId(0));
        entry.terminator = Some(Terminator::ShortCircuit {
            operand: Operand::Copy(Place::Local(Local(1))),
            kind: ShortCircuitKind::And,
            destination: Place::Local(Local(2)),
            eval_rhs: BlockId(1),
            join: BlockId(2),
        });
        let mut rhs = BasicBlock::new(BlockId(1));
        rhs.statements.push(Statement {
            kind: StatementKind::Assign {
                destination: Place::Local(Local(2)),
                value: Rvalue::Use(Operand::Constant(Constant::Bool(true))),
            },
            span: None,
        });
        rhs.terminator = Some(Terminator::Goto { target: BlockId(2) });
        let mut join = BasicBlock::new(BlockId(2));
        join.terminator = Some(Terminator::Return);
        let body = MirFunctionBody {
            blocks: vec![entry, rhs, join],
            entry: BlockId(0),
            locals: (0..3)
                .map(|_| LocalDecl {
                    name: None,
                    ty: RuntimeTy::Bool,
                    span: None,
                    scope_span: None,
                    is_captured: false,
                })
                .collect(),
        };
        let states = initialized_on_entry(&body, 1).unwrap();
        assert!(
            !states[1].as_ref().unwrap()[2],
            "RHS does not receive the short-edge assignment"
        );
        assert!(
            states[2].as_ref().unwrap()[2],
            "both paths assign before the join"
        );
        let callees = HashMap::new();
        let emitter = Emitter {
            types: &[NativeType::Bool; 3],
            callees: &callees,
            initialized: states[1].clone().unwrap(),
            direct: &HashMap::new(),
            resumable: true,
        };
        assert!(matches!(emitter.read(Local(2)), Err(Rejection::Invalid(_))));
    }
}
