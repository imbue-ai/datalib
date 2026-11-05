// The one node API `remote-images-load.spec.ts` needs, declared rather than imported.
declare module "node:http" {
  export interface ServerResponse {
    writeHead(status: number, headers: Record<string, string>): void;
    end(body: Uint8Array): void;
  }
  export interface Server {
    listen(port: number, host: string, ready: () => void): void;
    address(): { port: number };
    close(): void;
  }
  export function createServer(handler: (req: unknown, res: ServerResponse) => void): Server;
}
