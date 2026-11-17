"""Gradio fixture that never calls launch(): it only defines `demo`, the
shape the adapter must detect and launch itself."""

import gradio as gr

print("w3-entry-ran demo-only", flush=True)


def flip(text):
    return "flipped " + text


demo = gr.Interface(
    flip,
    inputs=gr.Text(),
    outputs=gr.Text(),
    title="pylair-wrap-gradio-marker",
)
