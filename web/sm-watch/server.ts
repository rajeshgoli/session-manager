import express from "express";
import { createServer as createHttpServer } from "http";
import { WebSocketServer, WebSocket } from "ws";
import { createServer as createViteServer } from "vite";
import path from "path";
import { SessionStatus } from "./src/types";

async function startServer() {
  const app = express();
  const httpServer = createHttpServer(app);
  const wss = new WebSocketServer({ server: httpServer });
  const PORT = 3000;

  // Mock session data generator
  const generateSessions = () => {
    const statuses: SessionStatus[] = ["running", "idle", "error", "detached", "active"];
    const owners = ["alice", "bob", "charlie", "system"];
    const humanStatuses = [
      "reviewing PR #1764 diff for edge-case coverage and spec adherence; preparing blocking feedback",
      "Engineer dispatched. #1763 tally: Round 1 blocked, Claude=eng / Codex=arch. Going idle.",
      "Sautéed for 32s",
      "indexing repository for semantic search",
      "executing test suite: 42/120 passed",
      "waiting for user input on architectural decision",
      "bypassing permissions on 2 files +0 -0 · PR #1764"
    ];

    const logs = [
      "Last login: Sat Feb 21 13:08:21 on ttys059",
      "rajesh@Rajeshs-MacBook-Pro fractal-market-simulator % sm watch",
      "⏺ Engineer dispatched. #1763 tally: Round 1 blocked, Claude=eng / Codex=arch. Going idle.",
      "✻ Sautéed for 32s",
      "──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────",
      "❯ ",
      "──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────",
      "  49% ctx",
      "  ⏵⏵ bypass permissions on (shift+tab to cycle) · 2 files +0 -0 · PR #1764"
    ];

    const baseSessions = Array.from({ length: 8 }, (_, i) => ({
      id: `sess-${Math.random().toString(36).substr(2, 9)}`,
      name: `Session ${i + 1}`,
      status: statuses[Math.floor(Math.random() * statuses.length)],
      owner: owners[Math.floor(Math.random() * owners.length)],
      createdAt: new Date(Date.now() - Math.random() * 10000000).toISOString(),
      lastHeartbeat: new Date().toISOString(),
      telegramThreadId: "123456789",
      priority: Math.random() > 0.8 ? "critical" : Math.random() > 0.6 ? "high" : "normal",
      humanStatus: humanStatuses[Math.floor(Math.random() * humanStatuses.length)],
      logs: logs,
      contextUsage: Math.floor(Math.random() * 100),
      isCompleted: Math.random() > 0.8,
    }));

    // Add some children
    const children = baseSessions.slice(0, 4).map((parent, i) => ({
      id: `child-${Math.random().toString(36).substr(2, 9)}`,
      name: `Sub-agent ${i + 1}`,
      status: statuses[Math.floor(Math.random() * statuses.length)],
      owner: parent.owner,
      createdAt: new Date().toISOString(),
      lastHeartbeat: new Date().toISOString(),
      telegramThreadId: "123456789",
      priority: "normal",
      parentId: parent.id,
      humanStatus: "Assisting parent agent with sub-task",
      logs: logs,
      contextUsage: Math.floor(Math.random() * 50),
      isCompleted: Math.random() > 0.9,
    }));

    return [...baseSessions, ...children];
  };

  let sessions = generateSessions();

  // WebSocket connection handling
  wss.on("connection", (ws) => {
    console.log("Client connected");
    ws.send(JSON.stringify({ type: "INITIAL_STATE", data: sessions }));

    const interval = setInterval(() => {
      // Randomly update a session status or heartbeat
      sessions = sessions.map(s => {
        if (Math.random() > 0.8) {
          const statuses: SessionStatus[] = ["running", "idle", "error", "detached", "active"];
          return { ...s, status: statuses[Math.floor(Math.random() * statuses.length)], lastHeartbeat: new Date().toISOString() };
        }
        return { ...s, lastHeartbeat: new Date().toISOString() };
      });
      ws.send(JSON.stringify({ type: "UPDATE", data: sessions }));
    }, 5000);

    ws.on("close", () => clearInterval(interval));
  });

  // API Routes
  app.get("/api/health", (req, res) => {
    res.json({ status: "ok" });
  });

  app.post("/api/sessions/:id/kill", (req, res) => {
    const { id } = req.params;
    sessions = sessions.filter(s => s.id !== id);
    // Broadcast update
    wss.clients.forEach(client => {
      if (client.readyState === WebSocket.OPEN) {
        client.send(JSON.stringify({ type: "UPDATE", data: sessions }));
      }
    });
    res.json({ success: true });
  });

  // Vite middleware for development
  if (process.env.NODE_ENV !== "production") {
    const vite = await createViteServer({
      server: { middlewareMode: true },
      appType: "spa",
    });
    app.use(vite.middlewares);
  } else {
    app.use(express.static(path.join(process.cwd(), "dist")));
    app.get("*", (req, res) => {
      res.sendFile(path.join(process.cwd(), "dist", "index.html"));
    });
  }

  httpServer.listen(PORT, "0.0.0.0", () => {
    console.log(`Server running on http://localhost:${PORT}`);
  });
}

startServer();
