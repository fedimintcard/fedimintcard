package io.github.bobodread876.fedimintcard.bridge;

import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Just enough JSON for {"apdu":"<hex>"}. */
final class Json {
    private static final Pattern APDU = Pattern.compile(
        "^\\s*\\{\\s*\"apdu\"\\s*:\\s*\"([^\"\\\\]*)\"\\s*\\}\\s*$");

    private Json() {}

    /** Returns the apdu string, or null when the body is not exactly that object. */
    static String extractApdu(String body) {
        Matcher m = APDU.matcher(body);
        return m.matches() ? m.group(1) : null;
    }

    static String response(String hex) { return "{\"response\":\"" + hex + "\"}"; }

    static String error(String msg) {
        return "{\"error\":\"" + msg.replace("\\", "\\\\").replace("\"", "\\\"") + "\"}";
    }

    static final String OK = "{\"ok\":true}";
}
