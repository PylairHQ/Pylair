import { createRoot } from "react-dom/client";
import { maybeSelfTest } from "@pylair/client";
import App from "./App";
import "./app.css";

createRoot(document.getElementById("root")!).render(<App />);
void maybeSelfTest();
