package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.TearHarnessTest.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * An anchor torn after each of its writes in turn. The clock record is the old one or the anchor's, never a mix; nothing
 * is held after the power-up, so SET_PIN then asks for a new anchor, which goes.
 */
class AnchorTearTest {

    /** A card with no PIN anchored at block 900 200, with a window and a count in its clock record. */
    static Card anchoredLater() throws Exception {
        Card c = new Card();
        assertEquals(0x9000, c.tx(0x34, 0, TEST_REGISTRY_KEY).getSW());
        assertEquals(0x9000, c.tx(0x19, 0, Headers.H900200).getSW());
        byte[] clock = (byte[]) c.field("clock");
        // REFLECTION: a window and a count, so the anchor's clearing shows in the record
        Arrays.fill(clock, 4, 8, (byte) 0x11);
        Arrays.fill(clock, 20, 28, (byte) 0x22);
        c.select();
        return c;
    }

    static boolean zero(byte[] a) {
        for (byte b : a) if (b != 0) return false;
        return true;
    }

    @Test
    void anAnchorTornAnywhereLeavesTheOldClockOrTheAnchorsNeverAMix() throws Exception {
        Card c = anchoredLater();
        byte[] before = ((byte[]) c.field("clock")).clone();
        TearHook.count = 0;
        assertEquals(0x9000, c.tx(0x19, 0, Headers.H900000).getSW());
        int total = TearHook.count;
        byte[] after = ((byte[]) c.field("clock")).clone();
        assertEquals(Headers.T900000, OfflineTest.u32(after, 0), "the clock at the anchor's time, earlier");
        assertTrue(zero(Arrays.copyOfRange(after, 4, 8)) && zero(Arrays.copyOfRange(after, 20, 28)), "window and count cleared");
        assertArrayEquals(Arrays.copyOfRange(before, 8, 20), Arrays.copyOfRange(after, 8, 20), "the hardest and the limit kept");
        System.out.println("anchor tear sweep: " + total + " writes, each torn");
        List<String> wrong = new ArrayList<>();
        int old = 0, anchored = 0;
        for (int k = 1; k <= total; k++) {
            Card t = anchoredLater();
            TearHook.count = 0;
            TearHook.target = k;
            int r = t.tx(0x19, 0, Headers.H900000).getSW();
            String at = TearHook.tornAt;
            TearHook.target = 0;
            if (at == null) { wrong.add("write " + k + " was not reached"); continue; }
            if (r == 0x9000) wrong.add("write " + k + ": a torn anchor answered 9000");
            t.powerCycle();
            byte[] clock = (byte[]) t.field("clock");
            if (Arrays.equals(clock, before)) old++;
            else if (Arrays.equals(clock, after)) anchored++;
            else wrong.add("write " + k + " (" + at + "): a mixed clock record " + Arrays.toString(clock));
            if (!zero((byte[]) t.field("held"))) wrong.add("write " + k + ": something held after the power-up");
            if (t.tx(SET_PIN, 0, PIN).getSW() != 0x6A96) wrong.add("write " + k + ": SET_PIN without an anchor in its selection");
            if (t.tx(0x19, 0, Headers.H900000).getSW() != 0x9000) wrong.add("write " + k + ": no anchor after the tear");
            if (t.tx(SET_PIN, 0, PIN).getSW() != 0x9000) wrong.add("write " + k + ": SET_PIN after the anchor");
        }
        assertEquals(List.of(), wrong);
        assertTrue(old >= 1 && anchored >= 1, old + " old, " + anchored + " anchored");
    }
}
