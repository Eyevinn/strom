//! Share a flow's GL context with a pipeline a block runs of its own.
//!
//! A pipeline shares contexts among its elements: an element that needs one
//! posts `NEED_CONTEXT`, and the bin answers from the contexts it has cached
//! from earlier `HAVE_CONTEXT` messages. Two pipelines share nothing, so a
//! block that decodes in a pipeline of its own and hands the frames to the
//! flow over an appsink/appsrc pair ends up with two GL contexts. GL memory is
//! only usable in the context that made it, and the first GL element in the
//! flow that converts such a frame fails:
//!
//! ```text
//! glcolorconvert0 input memory OpenGL context is different. we have <glcontext…1> memory has <glcontext…0>
//! ```
//!
//! [`install`] posts the internal pipeline's `NEED_CONTEXT` and
//! `HAVE_CONTEXT` messages again from an element in the flow. The flow's bin
//! then answers the internal pipeline's requests from its cache and caches
//! what the internal pipeline creates, so both sides use the same context,
//! whichever pipeline asks first. webrtcsink does the same for its session
//! pipelines.
//!
//! Only GL context types go through. CUDA and D3D memory cross between
//! pipelines without a shared context (measured, #1099). Their contexts are
//! tied to a device, and a bin caches one context per type, so forwarding an
//! internal decoder's CUDA context or D3D device would replace the flow's own
//! on a host where the two use different GPUs.
//!
//! Two flows cannot share a context this way: each starts and stops on its
//! own. See issue #1099.

use gstreamer as gst;
use gstreamer::prelude::*;

/// Hand `internal`'s GL context requests and GL contexts to the pipeline that
/// `flow_element` is in.
///
/// Holds `flow_element` weakly: once the flow is torn down, the messages stay
/// in `internal` as if this were never installed. A message posted while
/// `flow_element` has no parent yet reaches no bin, and the internal pipeline
/// makes its own context, as it would without this.
///
/// Installs `internal`'s bus sync handler. A bus has only one, so `internal`
/// must not need another.
pub fn install(internal: &gst::Pipeline, flow_element: &impl IsA<gst::Element>) {
    let Some(bus) = internal.bus() else {
        return;
    };
    let target = flow_element.as_ref().downgrade();
    bus.set_sync_handler(move |_, msg| {
        let gl = match msg.view() {
            gst::MessageView::NeedContext(m) => is_gl_context_type(m.context_type()),
            gst::MessageView::HaveContext(m) => is_gl_context_type(m.context().context_type()),
            _ => false,
        };
        if gl {
            if let Some(target) = target.upgrade() {
                let _ = target.post_message(msg.clone());
            }
        }
        gst::BusSyncReply::Pass
    });
}

/// The context types GStreamer's GL library asks for and posts: the GL
/// display, the application's and the local GL context, and the window
/// system display handles a GL display is made from.
fn is_gl_context_type(context_type: &str) -> bool {
    context_type.starts_with("gst.gl.")
        || context_type == "gst.x11.display.handle"
        || context_type == "GstWaylandDisplayHandleContextType"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GL context type, carrying a test value instead of a display: no GL
    /// element is in these pipelines to read it.
    const CONTEXT_TYPE: &str = "gst.gl.app_context";

    fn context(value: &str) -> gst::Context {
        let mut context = gst::Context::new(CONTEXT_TYPE, true);
        context
            .get_mut()
            .unwrap()
            .structure_mut()
            .set("value", value);
        context
    }

    fn value(context: Option<gst::Context>) -> Option<String> {
        context.and_then(|c| c.structure().get::<String>("value").ok())
    }

    /// A flow with one element in it, and an internal pipeline with one
    /// element in it, wired with [`install`].
    fn rig() -> (gst::Pipeline, gst::Pipeline, gst::Element) {
        gst::init().unwrap();
        let flow = gst::Pipeline::new();
        let flow_element = gst::ElementFactory::make("fakesink").build().unwrap();
        flow.add(&flow_element).unwrap();
        let internal = gst::Pipeline::new();
        let inner = gst::ElementFactory::make("fakesrc").build().unwrap();
        internal.add(&inner).unwrap();
        install(&internal, &flow_element);
        (flow, internal, inner)
    }

    #[test]
    fn a_request_from_inside_is_answered_by_the_flow() {
        let (flow, _internal, inner) = rig();
        flow.set_context(&context("from the flow"));

        inner
            .post_message(
                gst::message::NeedContext::builder(CONTEXT_TYPE)
                    .src(&inner)
                    .build(),
            )
            .unwrap();

        assert_eq!(
            value(inner.context(CONTEXT_TYPE)),
            Some("from the flow".into())
        );
    }

    #[test]
    fn a_context_made_inside_is_kept_by_the_flow() {
        let (flow, _internal, inner) = rig();

        inner
            .post_message(
                gst::message::HaveContext::builder(context("from inside"))
                    .src(&inner)
                    .build(),
            )
            .unwrap();

        assert_eq!(
            value(flow.context(CONTEXT_TYPE)),
            Some("from inside".into())
        );
    }

    #[test]
    fn a_device_context_stays_inside() {
        let (flow, _internal, inner) = rig();
        const CUDA: &str = "gst.cuda.context";

        inner
            .post_message(
                gst::message::HaveContext::builder(gst::Context::new(CUDA, true))
                    .src(&inner)
                    .build(),
            )
            .unwrap();

        assert!(
            flow.context(CUDA).is_none(),
            "the flow keeps its own device context"
        );
    }

    #[test]
    fn a_torn_down_flow_leaves_the_messages_inside() {
        let (flow, internal, inner) = rig();
        drop(flow);

        inner
            .post_message(
                gst::message::NeedContext::builder(CONTEXT_TYPE)
                    .src(&inner)
                    .build(),
            )
            .unwrap();

        assert_eq!(value(inner.context(CONTEXT_TYPE)), None);
        let msg = internal
            .bus()
            .unwrap()
            .pop_filtered(&[gst::MessageType::NeedContext]);
        assert!(msg.is_some(), "the request still reaches the internal bus");
    }
}
