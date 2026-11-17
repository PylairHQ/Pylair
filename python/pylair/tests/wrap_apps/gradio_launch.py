"""Gradio fixture for the adapter tests: calls demo.launch() with
arguments the adapter must override. If any of them survived, the server
would bind a public interface, open a browser, tunnel out, or free the
main thread - every one is a failure mode the adapter exists to kill."""

import os

import gradio as gr

print(
    f"w3-entry-ran analytics={os.environ.get('GRADIO_ANALYTICS_ENABLED')}", flush=True
)

with gr.Blocks(title="w3 gradio") as demo:
    gr.Markdown("# pylair-wrap-gradio-marker")

demo.launch(
    server_name="0.0.0.0",
    server_port=9999,
    inbrowser=True,
    share=True,
    prevent_thread_lock=True,
)
