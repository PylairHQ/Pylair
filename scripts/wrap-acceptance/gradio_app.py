"""Gradio acceptance fixture: the page title is the marker the self-test looks
for in `GET /` (gradio embeds it in the served config)."""

import gradio as gr

demo = gr.Interface(lambda name: "hello " + name, "text", "text", title="PYLAIR-WRAP-OK")
demo.launch()
