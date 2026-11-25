package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import javacard.security.ECPublicKey;
import org.junit.jupiter.api.Test;

/**
 * The registry's own vectors (registry/test/note-sig-vectors.json, made by its signer under a test key) are the
 * contract: the card takes every valid case and refuses the invalid one. Each case's slot key is put on the card's
 * slot by reflection (the card reads it from there); everything else is sent as a till sends it.
 */
class NoteSignatureVectorsTest {
    static final Path FILE = Path.of("..", "registry", "test", "note-sig-vectors.json");

    static List<Map<String, String>> cases(String json) {
        List<Map<String, String>> out = new ArrayList<>();
        Matcher obj = Pattern.compile("\\{[^{}]*\"valid\"[^{}]*\\}").matcher(json);
        while (obj.find()) {
            Map<String, String> m = new LinkedHashMap<>();
            Matcher kv = Pattern.compile("\"(\\w+)\"\\s*:\\s*(\"([^\"]*)\"|true|false|\\d+)").matcher(obj.group());
            while (kv.find()) m.put(kv.group(1), kv.group(3) != null ? kv.group(3) : kv.group(2));
            out.add(m);
        }
        return out;
    }

    @Test
    void theRegistrysVectorsAreTheContract() throws Exception {
        String json = Files.readString(FILE);
        Matcher pk = Pattern.compile("\"public_key\"\\s*:\\s*\"([0-9a-f]{130})\"").matcher(json);
        assertTrue(pk.find());
        List<Map<String, String>> cases = cases(json);
        assertEquals(5, cases.size());
        int valid = 0, invalid = 0;
        for (Map<String, String> v : cases) {
            byte[] x = Bip340.hex(v.get("slot_xonly"));
            byte[] note = Bip340.hex(v.get("amount_be") + v.get("fed_sig"));
            assertArrayEquals(Bip340.hex(v.get("message")), Registry.noteMessage(x, note), v.get("name") + ": the message");

            CardTestSupport c = new CardTestSupport();
            assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, Bip340.hex(pk.group(1))));
            assertSw(0x9000, c.tx(GEN_KEY, 5, null));
            byte[] w = new byte[65];
            w[0] = 0x04;
            System.arraycopy(x, 0, w, 1, 32);
            ((ECPublicKey[]) c.field("pubs"))[5].setW(w, (short) 0, (short) 65);
            assertArrayEquals(x, c.slotX(5));

            byte[] data = Bip340.hex(v.get("amount_be") + v.get("fed_sig") + v.get("reg_sig"));
            if (v.get("valid").equals("true")) {
                assertSw(0x9000, c.tx(SET_NOTE, 5, data), v.get("name"));
                assertEquals(ST_NOTE, c.slotState(5));
                valid++;
            } else {
                assertSw(0x6982, c.tx(SET_NOTE, 5, data), v.get("name"));
                assertEquals(ST_KEYED, c.slotState(5));
                invalid++;
            }
        }
        assertEquals(4, valid);
        assertEquals(1, invalid);
    }
}
