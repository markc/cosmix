use layout::backends::svg::SVGWriter;
use layout::gv;

/// Render a DOT graph string to an SVG string.
pub fn render_dot(dot_source: &str) -> Result<String, String> {
    let mut parser = gv::DotParser::new(dot_source);
    let graph = parser.process().map_err(|e| format!("DOT parse error: {e}"))?;

    let mut builder = gv::GraphBuilder::new();
    builder.visit_graph(&graph);
    let mut vg = builder.get();

    let mut svg = SVGWriter::new();
    vg.do_it(false, false, false, &mut svg);

    Ok(svg.finalize())
}
