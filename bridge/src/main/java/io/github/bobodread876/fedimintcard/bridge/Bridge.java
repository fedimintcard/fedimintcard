package io.github.bobodread876.fedimintcard.bridge;

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.BindException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.Executors;

/** HTTP bridge to the simulated applet, on 127.0.0.1 only. */
public final class Bridge {
    static final int MAX_BODY = 1024;

    private final HttpServer server;
    private final SimCard card;

    public Bridge(int port, boolean fixedKeys) throws IOException {
        card = new SimCard(fixedKeys);
        server = HttpServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), port), 0);
        server.setExecutor(Executors.newSingleThreadExecutor());
        server.createContext("/", this::route);
    }

    public void start() { server.start(); }

    public void stop() { server.stop(0); }

    public int port() { return server.getAddress().getPort(); }

    public InetSocketAddress address() { return server.getAddress(); }

    private void route(HttpExchange ex) throws IOException {
        try {
            String path = ex.getRequestURI().getPath();
            String method = ex.getRequestMethod();
            switch (path) {
                case "/health":
                    if (!method.equals("GET")) { send(ex, 405, Json.error("method not allowed")); return; }
                    send(ex, 200, Json.OK);
                    return;
                case "/reset":
                    if (!method.equals("POST")) { send(ex, 405, Json.error("method not allowed")); return; }
                    readBody(ex);
                    card.reset();
                    send(ex, 200, Json.OK);
                    return;
                case "/apdu":
                    if (!method.equals("POST")) { send(ex, 405, Json.error("method not allowed")); return; }
                    apdu(ex);
                    return;
                default:
                    send(ex, 404, Json.error("not found"));
            }
        } catch (RuntimeException e) {
            try {
                send(ex, 500, Json.error("internal error"));
            } catch (IOException | RuntimeException again) {
                // headers may already have been sent; nothing more to do but drop the exchange
                ex.close();
            }
        }
    }

    private void apdu(HttpExchange ex) throws IOException {
        String body = readBody(ex);
        if (body == null) { send(ex, 400, Json.error("body too large")); return; }
        String hex = Json.extractApdu(body);
        if (hex == null) { send(ex, 400, Json.error("expected {\"apdu\":\"<hex>\"}")); return; }
        if (hex.length() % 2 != 0 || !hex.matches("[0-9a-fA-F]*")) {
            send(ex, 400, Json.error("apdu must be an even number of hex digits"));
            return;
        }
        if (hex.length() < 8) { send(ex, 400, Json.error("apdu shorter than 4 bytes")); return; }
        byte[] cmd = new byte[hex.length() / 2];
        for (int i = 0; i < cmd.length; i++) {
            cmd[i] = (byte) Integer.parseInt(hex.substring(2 * i, 2 * i + 2), 16);
        }
        byte[] resp;
        try {
            resp = card.transmit(cmd);
        } catch (IllegalArgumentException e) {
            send(ex, 400, Json.error("malformed apdu"));
            return;
        }
        StringBuilder sb = new StringBuilder();
        for (byte b : resp) sb.append(String.format("%02x", b));
        send(ex, 200, Json.response(sb.toString()));
    }

    /** Reads at most MAX_BODY bytes; null when the body is larger. */
    private static String readBody(HttpExchange ex) throws IOException {
        try (InputStream in = ex.getRequestBody()) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[256];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
                if (out.size() > MAX_BODY) return null;
            }
            return out.toString(StandardCharsets.UTF_8);
        }
    }

    private static void send(HttpExchange ex, int code, String json) throws IOException {
        byte[] b = json.getBytes(StandardCharsets.UTF_8);
        ex.getResponseHeaders().set("Content-Type", "application/json");
        ex.sendResponseHeaders(code, b.length);
        ex.getResponseBody().write(b);
        ex.close();
    }

    public static void main(String[] args) throws Exception {
        int port = 9876;
        boolean fixed = false;
        for (int i = 0; i < args.length; i++) {
            if (args[i].equals("--port") && i + 1 < args.length) port = Integer.parseInt(args[++i]);
            else if (args[i].equals("--fixed-keys")) fixed = true;
            else { System.err.println("usage: bridge [--port N] [--fixed-keys]"); System.exit(2); }
        }
        Bridge b;
        try {
            b = new Bridge(port, fixed);
        } catch (BindException e) {
            System.err.println("port " + port + " is in use");
            System.exit(2);
            return;
        }
        b.start();
        System.out.println("listening on 127.0.0.1:" + b.port());
    }
}
