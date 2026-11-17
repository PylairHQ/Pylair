"""Streamlit acceptance fixture. Streamlit renders the page over a websocket,
so `GET /` is the static shell, whose title is "Streamlit": that is the marker
the self-test checks for this framework."""

import streamlit as st

st.title("PYLAIR-WRAP-OK")
