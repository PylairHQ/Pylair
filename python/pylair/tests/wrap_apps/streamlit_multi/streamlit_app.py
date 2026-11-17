"""Streamlit fixture: multipage app. The `pages/` dir next to this entry
is what Streamlit discovers; the test GETs the second page's URL."""

import streamlit as st

print("w3-entry-ran multipage", flush=True)

st.set_page_config(page_title="w3 streamlit multi")
st.title("pylair-wrap-streamlit-marker")
st.write("main page")
