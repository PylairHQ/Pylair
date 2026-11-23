onmessage = async (e) => {
  try {
    const r = await fetch(e.data);
    const len = (await r.arrayBuffer()).byteLength;
    postMessage({ status: r.status, len });
  } catch (err) {
    postMessage({ status: -1, err: String(err) });
  }
};
