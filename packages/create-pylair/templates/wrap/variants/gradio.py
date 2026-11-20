# A minimal Gradio app wrapped as a desktop app by Pylair. It exists so
# `npx pylair dev` opens a real window out of the box; replace it with
# your own app (the PYLAIR-WRAP-OK title is only a smoke-test marker).

import gradio as gr

demo = gr.Interface(lambda name: "hello " + name, "text", "text", title="PYLAIR-WRAP-OK")
demo.launch()
