package io.github.bobodread876.fedimintcard.bridge;

import static org.junit.jupiter.api.Assertions.*;

import io.github.bobodread876.fedimintcard.applet.Bip340;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class BridgeHttpTest {
    Bridge bridge;
    HttpClient http = HttpClient.newHttpClient();
    String base;

    @BeforeEach
    void up() throws Exception {
        bridge = new Bridge(0, false);
        bridge.start();
        base = "http://127.0.0.1:" + bridge.port();
    }

    @AfterEach
    void down() { bridge.stop(); }

    HttpResponse<String> call(String method, String path, String body) throws Exception {
        HttpRequest.Builder b = HttpRequest.newBuilder(URI.create(base + path));
        b.method(method, body == null
            ? HttpRequest.BodyPublishers.noBody() : HttpRequest.BodyPublishers.ofString(body));
        return http.send(b.build(), HttpResponse.BodyHandlers.ofString());
    }

    /** Returns the response hex (data || SW). */
    String apdu(String hex) throws Exception {
        HttpResponse<String> r = call("POST", "/apdu", "{\"apdu\":\"" + hex + "\"}");
        assertEquals(200, r.statusCode(), r.body());
        Matcher m = Pattern.compile("\\{\"response\":\"([0-9a-f]*)\"\\}").matcher(r.body());
        assertTrue(m.matches(), r.body());
        return m.group(1);
    }

    @Test
    void healthAndLoopback() throws Exception {
        HttpResponse<String> r = call("GET", "/health", null);
        assertEquals(200, r.statusCode());
        assertEquals("{\"ok\":true}", r.body());
        assertTrue(bridge.address().getAddress().isLoopbackAddress());
    }

    @Test
    void selectGenKeySignVerifiesOverHttp() throws Exception {
        assertEquals("9000", apdu("00a4040008f0464d4341524401"));
        String gen = apdu("b0100000");
        assertEquals(68, gen.length());
        assertTrue(gen.endsWith("9000"));
        byte[] pub = Bip340.hex(gen.substring(0, 64));
        byte[] msg = new byte[32];
        for (int i = 0; i < 32; i++) msg[i] = (byte) (i * 5);
        String sig = apdu("b0120000" + "20" + Bip340.hex(msg));
        assertEquals(64 * 2 + 4, sig.length());
        assertTrue(sig.endsWith("9000"));
        assertTrue(Bip340.verify(pub, msg, Bip340.hex(sig.substring(0, 128))));
        assertEquals(gen, apdu("b0110000"));
    }

    @Test
    void refusalIsStill200() throws Exception {
        assertEquals("6985", apdu("b0110000"));
    }

    @Test
    void resetEmptiesSlotsAndGivesNewKeys() throws Exception {
        String first = apdu("b0100000");
        assertEquals(200, call("POST", "/reset", "").statusCode());
        assertEquals("6985", apdu("b0110000"));
        String second = apdu("b0100000");
        assertNotEquals(first, second, "seeded: a reset card must not repeat the first key");
    }

    @Test
    void badInputIs400() throws Exception {
        for (String body : new String[] { "not json", "{\"apdu\":\"b01\"}", "{\"apdu\":\"zz100000\"}",
            "{\"apdu\":\"b01000\"}", "{\"apdu\":\"\"}", "{\"x\":\"b0100000\"}" }) {
            assertEquals(400, call("POST", "/apdu", body).statusCode(), body);
        }
        assertEquals(400, call("POST", "/apdu", "{\"apdu\":\"" + "00".repeat(600) + "\"}").statusCode());
    }

    @Test
    void routing() throws Exception {
        assertEquals(404, call("GET", "/nope", null).statusCode());
        assertEquals(405, call("GET", "/apdu", null).statusCode());
        assertEquals(405, call("POST", "/health", "").statusCode());
    }
}
