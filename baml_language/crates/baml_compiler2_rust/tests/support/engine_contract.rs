use bex_engine::{
    BexEngine, BexExternalValue as External, FunctionCallContextBuilder, TelemetryRecording,
};
use btel_recorder::{RecordingConfig, proto};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use sys_native::SysOpsExt;

fn context() -> bex_engine::FunctionCallContext {
    FunctionCallContextBuilder::new(sys_types::CallId::next()).build()
}
async fn logged(logger: &bex_engine::logger::TraceLogger) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while logger.stats().published == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("BAML did not reach the checkpoint");
}
async fn run(image: &bex_vm_types::Program, compiled: bool) -> Vec<String> {
    let mut program = image.clone();
    if compiled {
        generated::install(&mut program).unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let recording =
        TelemetryRecording::local_files_in(directory.path(), RecordingConfig::default());
    let engine = Arc::new(
        BexEngine::new_with_telemetry_recording(
            program,
            Arc::new(sys_native::SysOps::native()),
            vec![],
            None,
            btel_clock::ClockMode::Monotonic,
            recording,
        )
        .unwrap(),
    );
    let metadata = engine.program_metadata().await;
    assert_eq!(
        metadata
            .function_table
            .functions
            .iter()
            .any(|f| matches!(f.kind, btel_types::RuntimeFunctionKind::Compiled)),
        compiled
    );

    let logger = bex_engine::logger::TraceLogger::bounded(4);
    let done = AtomicBool::new(false);
    let execute = async {
        let result = engine
            .call_function(
                "user.compiled_entry",
                vec![External::Int(5)],
                FunctionCallContextBuilder::new(sys_types::CallId::next())
                    .with_logger(logger.clone())
                    .build(),
                true,
            )
            .await;
        done.store(true, Ordering::SeqCst);
        result.unwrap()
    };
    let collect = async {
        logged(&logger).await;
        assert!(!done.load(Ordering::SeqCst));
        engine
            .collect_garbage(bex_heap::CollectionLevel::Major)
            .await;
        assert!(
            !done.load(Ordering::SeqCst),
            "collection must finish while the call is suspended"
        );
    };
    let (result, ()) = tokio::join!(execute, collect);
    assert_eq!(result, External::Int(16));
    assert_eq!(
        engine
            .call_function("user.spawn_entry", vec![External::Int(4)], context(), true)
            .await
            .unwrap(),
        External::Int(13)
    );
    let error = engine
        .call_function(
            "user.failure_entry",
            vec![External::Int(0)],
            context(),
            true,
        )
        .await
        .unwrap_err()
        .to_string();

    let logger = bex_engine::logger::TraceLogger::bounded(4);
    let cancel = bex_engine::CancellationToken::new();
    let call = engine.call_function(
        "user.cancel_entry",
        vec![External::Int(3)],
        FunctionCallContextBuilder::new(sys_types::CallId::next())
            .with_logger(logger.clone())
            .with_cancel_token(cancel.clone())
            .build(),
        true,
    );
    let cancellation = async {
        logged(&logger).await;
        cancel.cancel();
    };
    let (cancelled, ()) = tokio::join!(call, cancellation);
    let cancelled = cancelled.unwrap_err().to_string();
    assert!(cancelled.contains("Cancelled"));

    let logger = bex_engine::logger::TraceLogger::bounded(4);
    let cancel = bex_engine::CancellationToken::new();
    let call = engine.call_function(
        "user.shield_entry",
        vec![],
        FunctionCallContextBuilder::new(sys_types::CallId::next())
            .with_logger(logger.clone())
            .with_cancel_token(cancel.clone())
            .build(),
        true,
    );
    let cancellation = async {
        logged(&logger).await;
        cancel.cancel();
    };
    let (shielded, ()) = tokio::join!(call, cancellation);
    assert_eq!(shielded.unwrap(), External::Int(7));
    assert_eq!(
        engine
            .call_function("user.captured", vec![External::Int(2)], context(), true)
            .await
            .unwrap(),
        External::Int(7)
    );
    engine.shutdown().await;
    assert_eq!(engine.telemetry_result(), Some(Ok(())));

    let mut functions = HashMap::new();
    let mut paths = HashMap::new();
    let mut announcements = HashMap::new();
    let mut completions = Vec::new();
    let mut raises = Vec::new();
    let mut saw_compiled_version = false;
    for file in btel_file::read_directory(engine.telemetry_recording_directory().unwrap())
        .unwrap()
        .files
    {
        saw_compiled_version |= file.header.as_ref().unwrap().format_minor >= 7;
        if let Some(definitions) = file.definitions {
            for function in definitions.functions {
                if let Some(proto::function_definition::Resolution::Metadata(metadata)) =
                    function.resolution
                {
                    functions.insert(function.function_id, metadata);
                }
            }
            for path in definitions.call_paths {
                paths.insert(path.call_path_id, path);
            }
        }
        if let Some(spans) = file.spans {
            for section in spans.sections {
                for event in section.events {
                    match event.event.unwrap() {
                        proto::span_event::Event::FunctionAnnouncement(start) => {
                            announcements.insert(start.id, start);
                        }
                        proto::span_event::Event::FunctionCompletion(done)
                        | proto::span_event::Event::LateFunctionCompletion(done) => {
                            completions.push(done)
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(errors) = file.errors {
            raises.extend(errors.raises);
        }
    }
    assert_eq!(saw_compiled_version, compiled);
    let mut invocations = Vec::new();
    let mut captured = false;
    for completion in &completions {
        let path = &paths[&((completion.node >> 1) as u32)];
        let function = &functions[&path.callee_function_id];
        if function.fqn.starts_with("user.") {
            let caller = path
                .visible_caller_function_id
                .and_then(|id| functions.get(&id))
                .map(|f| f.fqn.as_str());
            invocations.push(format!(
                "{}<-{caller:?}:{}:{}",
                function.fqn, completion.completion_flags, completion.panicked
            ));
            if function.fqn == "user.compiled_entry" {
                assert_eq!(completion.self_await_ticks, 0);
            }
        }
        if function.fqn == "user.compiled_entry"
            && announcements
                .get(&completion.id)
                .is_some_and(|a| a.inputs_cas_id.is_some())
            && completion.value_cas_id.is_some()
        {
            captured = true;
            invocations.push(format!(
                "capture:{:?}:{:?}",
                announcements[&completion.id].inputs_cas_id, completion.value_cas_id
            ));
        }
    }
    assert!(
        captured,
        "explicit captures must survive compiled entry/return"
    );
    let fail_line = BAML
        .lines()
        .position(|line| line.starts_with("function fail("))
        .unwrap() as u32
        + 1;
    let raise = raises
        .iter()
        .find(|raise| {
            raise
                .function_id
                .is_some_and(|id| functions.get(&id).is_some_and(|f| f.fqn == "user.fail"))
        })
        .expect("missing failure evidence");
    let function = &functions[&raise.function_id.unwrap()];
    let map = function.source_map.as_ref().unwrap();
    assert_eq!(
        map.coordinate,
        if compiled {
            proto::PcCoordinate::CompiledSite
        } else {
            proto::PcCoordinate::CompactByteOffset
        } as i32
    );
    let map = btel_reader::source_map::SourceMap::from_wire(
        map,
        function.source_span.as_ref().map(|span| span.file_id),
    )
    .unwrap();
    assert_eq!(map.resolve(raise.pc.unwrap()).unwrap().line, fail_line);
    for path in paths.values() {
        if let Some(caller) = path
            .visible_caller_function_id
            .and_then(|id| functions.get(&id))
            && caller.kind == proto::FunctionKind::Compiled as i32
        {
            let map = btel_reader::source_map::SourceMap::from_wire(
                caller.source_map.as_ref().unwrap(),
                caller.source_span.as_ref().map(|s| s.file_id),
            )
            .unwrap();
            assert!(
                map.resolve(path.caller_pc).is_ok(),
                "compiled call site must resolve"
            );
        }
    }
    invocations.sort();
    invocations.push(error);
    invocations.push(cancelled);
    invocations
}
#[tokio::main]
async fn main() {
    let image: bex_vm_types::Program = borsh::from_slice(include_bytes!("../program.bin")).unwrap();
    let baseline = tokio::time::timeout(Duration::from_secs(20), run(&image, false))
        .await
        .expect("interpreter stalled");
    let compiled = tokio::time::timeout(Duration::from_secs(20), run(&image, true))
        .await
        .expect("compiled execution stalled");
    assert_eq!(baseline, compiled);
    println!("engine contract ok");
}
