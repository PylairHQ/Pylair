"""Streamlit fixture: single page. The marker the test reads is the
printed line (the page body renders over the websocket, not in HTML)."""

import os

import streamlit as st

print(
    f"w3-entry-ran usageStats={os.environ.get('STREAMLIT_BROWSER_GATHER_USAGE_STATS')}",
    flush=True,
)

st.set_page_config(page_title="w3 streamlit")
st.title("pylair-wrap-streamlit-marker")
st.write("main page")
