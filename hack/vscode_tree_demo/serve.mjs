// Serves the demo page and the <vs-tree> bundle on localhost, and nothing
// else: the three files below, by their fixed names.
import fs from 'node:fs';
import http from 'node:http';

const FILES = {
  '/': ['./index.html', 'text/html; charset=utf-8'],
  '/demo.js': ['./demo.js', 'text/javascript'],
  '/vs-tree.js': ['../../third-party/vscode-tree/dist/vs-tree.js', 'text/javascript'],
};

const port = Number(process.argv[2] ?? 8765);
http
  .createServer((req, res) => {
    const file = FILES[new URL(req.url, 'http://x').pathname];
    if (!file) {
      res.writeHead(404);
      res.end();
      return;
    }
    res.writeHead(200, { 'content-type': file[1], 'cache-control': 'no-store' });
    res.end(fs.readFileSync(new URL(file[0], import.meta.url)));
  })
  .listen(port, '127.0.0.1', () => console.log(`vs-tree demo: http://127.0.0.1:${port}/`));
