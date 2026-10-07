//! Conservative admission for synchronous scalar call regions. The limits bound
//! work between existing cooperative checks and native stack usage; they are
//! implementation limits, not new BAML semantics. Rejected bodies stay resumable.

use std::collections::{HashMap, VecDeque};

use baml_compiler2_hir::loc::DeclRef;
use baml_compiler2_hir_ty::extern_loc::FunctionRef;
use baml_compiler2_mir::Terminator;
use bex_vm_types::{GlobalIndex, ObjectIndex};

use crate::{Admitted, DirectSupport, NativeType, direct_callee};

const MAX_WORK: usize = 256;
const MAX_DEPTH: usize = 16;
const MAX_LOCALS: usize = 128;

pub(crate) struct Target {
    pub object: ObjectIndex,
    pub global: GlobalIndex,
    pub parameters: Vec<NativeType>,
    pub result: NativeType,
    pub(crate) work: usize,
    depth: usize,
}

pub(crate) fn analyze<'db>(
    functions: &[Admitted<'db>],
) -> (HashMap<FunctionRef<'db>, Target>, Vec<DirectSupport>) {
    let mut pending = Vec::new();
    let mut reasons = HashMap::new();
    for function in functions {
        let reference = DeclRef::Source(function.loc);
        match local_bound(function) {
            Ok(work) => pending.push((function, work)),
            Err(reason) => {
                reasons.insert(reference, reason.to_owned());
            }
        }
    }
    let mut targets: HashMap<FunctionRef<'db>, Target> = HashMap::new();
    // Promote leaves, then callers of promoted functions. Recursive components
    // cannot enter this set, including cycles through otherwise scalar bodies.
    loop {
        let mut remaining = Vec::new();
        let mut progress = false;
        for (function, own_work) in pending {
            let reference = DeclRef::Source(function.loc);
            let mut work = own_work;
            let mut depth = 1;
            let mut ready = true;
            for block in &function.candidate.body.blocks {
                if let Some(call @ Terminator::Call { .. }) = &block.terminator {
                    let callee = direct_callee(call).expect("admission validated direct callee");
                    let Some(target) = targets.get(&callee) else {
                        ready = false;
                        break;
                    };
                    work = work.saturating_add(target.work);
                    depth = depth.max(target.depth + 1);
                }
            }
            if !ready {
                remaining.push((function, own_work));
                continue;
            }
            progress = true;
            if work > MAX_WORK || depth > MAX_DEPTH {
                reasons.insert(reference, format!(
                    "region exceeds direct bound ({work}/{MAX_WORK} work, {depth}/{MAX_DEPTH} depth)"
                ));
                continue;
            }
            targets.insert(
                reference,
                Target {
                    object: function.object,
                    global: function.global,
                    parameters: function.candidate.types[1..=function.candidate.arity].to_vec(),
                    result: function.candidate.types[0],
                    work,
                    depth,
                },
            );
        }
        pending = remaining;
        if !progress {
            break;
        }
    }
    for (function, _) in pending {
        reasons.insert(
            DeclRef::Source(function.loc),
            "calls a non-direct function (including recursive call cycles)".into(),
        );
    }
    let report = functions
        .iter()
        .map(|function| {
            let reference = DeclRef::Source(function.loc);
            let target = targets.get(&reference);
            DirectSupport {
                function: function.name.clone(),
                eligible: target.is_some(),
                reason: target.map_or_else(
                    || {
                        reasons
                            .remove(&reference)
                            .expect("every body has an admission result")
                    },
                    |target| {
                        format!(
                            "bounded scalar region: {} work units, {} calls deep",
                            target.work, target.depth
                        )
                    },
                ),
            }
        })
        .collect();
    (targets, report)
}

fn local_bound(function: &Admitted<'_>) -> Result<usize, &'static str> {
    let candidate = &function.candidate;
    if candidate.types.contains(&NativeType::IntArray) {
        return Err("heap-valued local, parameter or result");
    }
    if candidate.types.len() > MAX_LOCALS {
        return Err("more than 128 scalar locals");
    }
    let blocks = &candidate.body.blocks;
    let mut incoming = vec![0; blocks.len()];
    let edges: Vec<_> = blocks
        .iter()
        .map(|block| {
            block
                .terminator
                .as_ref()
                .expect("admitted terminator")
                .successors()
        })
        .collect();
    for block_edges in &edges {
        for target in block_edges {
            incoming[target.0] += 1;
        }
    }
    let mut queue: VecDeque<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(block, count)| (*count == 0).then_some(block))
        .collect();
    let mut visited = 0;
    while let Some(block) = queue.pop_front() {
        visited += 1;
        for target in &edges[block] {
            incoming[target.0] -= 1;
            if incoming[target.0] == 0 {
                queue.push_back(target.0);
            }
        }
    }
    if visited != blocks.len() {
        return Err("cyclic control flow requires cooperative checks");
    }
    // Sum both sides of branches deliberately. This overestimates execution,
    // but avoids a second path-sensitive cost analysis in the first experiment.
    let work = blocks.iter().fold(candidate.types.len(), |work, block| {
        work.saturating_add(block.statements.len())
            .saturating_add(1)
    });
    if work > MAX_WORK {
        return Err("body exceeds 256 direct work units");
    }
    Ok(work)
}
