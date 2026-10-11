// SPDX-License-Identifier: GPL-3.0-or-later
//! Compare CPU allocator settings on the already verified local model.
use ort::{execution_providers::CPUExecutionProvider, session::Session, value::Tensor};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || !["standard", "bounded"].contains(&args[1].as_str()) {
        return Err("usage: onnx_memory standard|bounded verified-model.onnx".into());
    }
    let bounded = args[1] == "bounded";
    let start = Instant::now();
    let builder = Session::builder()?.with_intra_threads(4)?;
    let builder = if bounded {
        builder
            .with_execution_providers([CPUExecutionProvider::default()
                .with_arena_allocator(false)
                .build()])?
            .with_memory_pattern(false)?
    } else {
        builder
    };
    let mut session = builder.commit_from_file(&args[2])?;
    for _ in 0..2 {
        let input = Tensor::from_array(ndarray::Array4::<f32>::zeros((1, 3, 1024, 1024)))?;
        let outputs = session.run(ort::inputs![input])?;
        let (_, output) = outputs.iter().next().ok_or("missing output")?;
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        if **shape != [1, 1, 1024, 1024] || data.len() != 1024 * 1024 {
            return Err("unexpected output shape".into());
        }
    }
    let status = std::fs::read_to_string("/proc/self/status")?;
    let value = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key)?.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    println!(
        "{{\"mode\":\"{}\",\"seconds\":{:.3},\"hwm_kib\":{},\"rss_kib\":{}}}",
        args[1],
        start.elapsed().as_secs_f64(),
        value("VmHWM:"),
        value("VmRSS:")
    );
    Ok(())
}
