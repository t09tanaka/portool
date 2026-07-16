import { createServer } from "node:http";

// Code-level fallback: with portool, WEB_PORT is this worktree's
// allocation; without it, the default applies.
const port = Number(process.env.WEB_PORT ?? 3000);

createServer((_req, res) => {
  res.end("hello from webapp\n");
}).listen(port, () => {
  console.log(`listening on http://localhost:${port}`);
});
