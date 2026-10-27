//! A consumer-side download, for tests that map the pixels of a producer that
//! emits GL memory (a GPU Vision Mixer).

use std::collections::HashMap;
use strom_types::{Element, Flow, Link, PropertyValue};

/// Link `from` to `to` through `gldownload` and a system-memory capsfilter,
/// named `{prefix}_dl` and `{prefix}_sys`. `gldownload` passes system memory
/// through, so this works behind a CPU producer too. The capsfilter is what
/// makes the download happen: `gldownload` also offers GL memory on its src
/// pad, and an `appsink` takes whatever it is offered.
pub fn link_through_download(flow: &mut Flow, prefix: &str, from: &str, to: &str) {
    let dl = format!("{prefix}_dl");
    let sys = format!("{prefix}_sys");
    flow.elements
        .push(element(&dl, "gldownload", HashMap::new()));
    flow.elements.push(element(
        &sys,
        "capsfilter",
        HashMap::from([(
            "caps".to_string(),
            PropertyValue::String("video/x-raw".to_string()),
        )]),
    ));
    for (from, to) in [
        (from.to_string(), format!("{dl}:sink")),
        (format!("{dl}:src"), format!("{sys}:sink")),
        (format!("{sys}:src"), to.to_string()),
    ] {
        flow.links.push(Link { from, to });
    }
}

fn element(id: &str, ty: &str, properties: HashMap<String, PropertyValue>) -> Element {
    Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties,
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}
