// Test support: a TCP proxy between a driver and PostgreSQL that reads the
// protocol messages going through it, to pin how a query is sent — how many
// round trips it costs, whether its statement is named (prepared, so
// reusable and eventually planned generically) and the parameter types its
// Parse declares. Nothing here is timed.

import net from "node:net";

/** A `Parse` the client sent. */
export interface ParseMessage {
  /** The statement's name; `""` is the unnamed statement. */
  name: string;
  /** The declared parameter type OIDs (0: unspecified). */
  oids: number[];
}

/** What the client sent, and the server answered, while something ran. */
export interface Sent {
  /**
   * The times the client sent after hearing from the server: each one is a
   * round trip it waited on. The server answers an extended-protocol
   * exchange only at its `Sync` or `Flush`, so a client that sends Parse,
   * Bind, Describe, Execute and Sync together counts one.
   */
  roundTrips: number;
  /** `Sync` and simple `Query` messages. */
  syncs: number;
  parses: ParseMessage[];
  /** A `Describe` of a statement was answered before the `Bind`: the client waited for the parameter types. */
  describedFirst: boolean;
  /** The parameter types the server reported (`ParameterDescription`), per Describe. */
  inferred: number[][];
  /** The message types the client sent, one string per round trip (`"PDH|BES"`). */
  flights: string[];
}

interface Log {
  flights: string[];
  syncs: number;
  parses: ParseMessage[];
  describedFirst: boolean;
  inferred: number[][];
}

const newLog = (): Log => ({ flights: [], syncs: 0, parses: [], describedFirst: false, inferred: [] });

class Connection {
  /** The server spoke since the client's last message: the next one starts a round trip. */
  private answered = true;
  private startup = true;
  private fromClient = Buffer.alloc(0);
  private fromServer = Buffer.alloc(0);
  /** A statement Describe went out in the current or an earlier round trip, with no Bind yet. */
  private describing = false;
  private describingFlight = -1;

  private wire: Wire;

  constructor(wire: Wire) {
    this.wire = wire;
  }

  client(chunk: Buffer): void {
    this.fromClient = Buffer.concat([this.fromClient, chunk]);
    for (;;) {
      const header = this.startup ? 4 : 5;
      if (this.fromClient.length < header) return;
      const len = this.fromClient.readUInt32BE(header - 4);
      const total = header - 4 + len;
      if (this.fromClient.length < total) return;
      const msg = this.fromClient.subarray(0, total);
      this.fromClient = this.fromClient.subarray(total);
      if (this.startup) {
        // SSLRequest / GSSENCRequest are followed by the real startup.
        const code = msg.readUInt32BE(4);
        if (code !== 80877103 && code !== 80877104) this.startup = false;
        continue;
      }
      this.message(String.fromCharCode(msg[0]), msg.subarray(5));
    }
  }

  private message(type: string, body: Buffer): void {
    const log = this.wire.log;
    if (this.answered) {
      log.flights.push("");
      this.answered = false;
    }
    const flight = log.flights.length - 1;
    log.flights[flight] += type;
    switch (type) {
      case "S":
      case "Q":
        log.syncs++;
        break;
      case "P": {
        const nameEnd = body.indexOf(0);
        const name = body.toString("utf8", 0, nameEnd);
        const textEnd = body.indexOf(0, nameEnd + 1);
        const count = body.readUInt16BE(textEnd + 1);
        const oids = [];
        for (let i = 0; i < count; i++) oids.push(body.readUInt32BE(textEnd + 3 + i * 4));
        log.parses.push({ name, oids });
        break;
      }
      case "D":
        if (body[0] === "S".charCodeAt(0)) {
          this.describing = true;
          this.describingFlight = flight;
        }
        break;
      case "B":
        if (this.describing && this.describingFlight < flight) log.describedFirst = true;
        this.describing = false;
        break;
    }
  }

  server(chunk: Buffer): void {
    this.answered = true;
    this.fromServer = Buffer.concat([this.fromServer, chunk]);
    for (;;) {
      // Before authentication completes the server may answer an
      // SSLRequest with one byte; the drivers here don't ask for TLS.
      if (this.fromServer.length < 5) return;
      const total = 1 + this.fromServer.readUInt32BE(1);
      if (this.fromServer.length < total) return;
      const msg = this.fromServer.subarray(0, total);
      this.fromServer = this.fromServer.subarray(total);
      if (msg[0] === "t".charCodeAt(0)) {
        const count = msg.readUInt16BE(5);
        const oids = [];
        for (let i = 0; i < count; i++) oids.push(msg.readUInt32BE(7 + i * 4));
        this.wire.log.inferred.push(oids);
      }
    }
  }
}

/** A proxy to the PostgreSQL at `upstream` (a connection URL). */
export class Wire {
  log: Log = newLog();
  private upstream: URL;
  private server: net.Server;
  private sockets = new Set<net.Socket>();

  private constructor(upstream: URL, server: net.Server) {
    this.upstream = upstream;
    this.server = server;
  }

  static async start(upstream: string): Promise<Wire> {
    const url = new URL(upstream);
    const server = net.createServer();
    const wire = new Wire(url, server);
    server.on("connection", (client) => wire.accept(client));
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    return wire;
  }

  /** The upstream URL, through the proxy. */
  get url(): string {
    const u = new URL(this.upstream);
    u.hostname = "127.0.0.1";
    u.port = String((this.server.address() as net.AddressInfo).port);
    return u.toString();
  }

  private accept(client: net.Socket): void {
    const conn = new Connection(this);
    const server = net.connect(Number(this.upstream.port || 5432), this.upstream.hostname);
    this.sockets.add(client).add(server);
    client.on("data", (chunk) => {
      // Read before the server sees it: by the time the client has its
      // answer, its messages are logged.
      conn.client(chunk);
      server.write(chunk);
    });
    server.on("data", (chunk) => {
      conn.server(chunk);
      client.write(chunk);
    });
    const close = () => {
      client.destroy();
      server.destroy();
      this.sockets.delete(client);
      this.sockets.delete(server);
    };
    client.on("close", close).on("error", close);
    server.on("close", close).on("error", close);
  }

  /** What went through while `run` ran. */
  async sent<T>(run: () => Promise<T>): Promise<[T, Sent]> {
    this.log = newLog();
    const out = await run();
    const log = this.log;
    this.log = newLog();
    return [
      out,
      {
        roundTrips: log.flights.length,
        syncs: log.syncs,
        parses: log.parses,
        describedFirst: log.describedFirst,
        inferred: log.inferred,
        flights: log.flights,
      },
    ];
  }

  async close(): Promise<void> {
    for (const s of this.sockets) s.destroy();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }
}
