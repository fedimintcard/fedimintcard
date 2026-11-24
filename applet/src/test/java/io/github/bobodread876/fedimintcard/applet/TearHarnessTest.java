package io.github.bobodread876.fedimintcard.applet;

import static org.junit.jupiter.api.Assertions.*;

import com.licel.jcardsim.base.SimulatorRuntime;
import com.licel.jcardsim.smartcardio.CardSimulator;
import com.licel.jcardsim.utils.AIDUtil;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import javacard.framework.AID;
import javacard.framework.Applet;
import javax.smartcardio.CommandAPDU;
import javax.smartcardio.ResponseAPDU;
import javacard.security.KeyPair;
import org.junit.jupiter.api.Test;

/**
 * The tear harness on this applet: SIGN on a note, with the card torn after each of its persistent writes in turn.
 * Every torn card holds the note (the slot still a note, nothing stored, and the next SIGN signs) or the spend (the
 * slot SPENT with a signature that verifies, answered again from storage). Nothing in between.
 */
class TearHarnessTest {

    static final String AID_HEX = "F0464D4341524401";
    static final int CLA = 0xB0, GEN_KEY = 0x10, SIGN = 0x12, SET_NOTE = 0x14, VERIFY_PIN = 0x40, SET_PIN = 0x41;
    static final int ST_NOTE = 2, ST_SPENT = 4;
    static final byte[] PIN = { '1', '2', '3', '4' };
    static final byte[] MSG = Bip340.hex("0101010101010101010101010101010101010101010101010101010101010101");

    static final class RT extends SimulatorRuntime {
        Applet appletAt(AID aid) { return getApplet(aid); }
        void clearTx() { transactionDepth = 0; }
    }

    static Class<? extends Applet> cls;

    @SuppressWarnings("unchecked")
    static synchronized Class<? extends Applet> cls() throws Exception {
        if (cls == null) {
            Path classes = Paths.get(FedimintCardApplet.class.getProtectionDomain().getCodeSource().getLocation().toURI());
            TearLoader l = new TearLoader(TearHarnessTest.class.getClassLoader(), classes);
            cls = (Class<? extends Applet>) l.loadClass(FedimintCardApplet.class.getName());
        }
        return cls;
    }

    /** One card with the rewritten applet. */
    static final class Card {
        final RT rt = new RT();
        final CardSimulator sim = new CardSimulator(rt);
        final AID aid = AIDUtil.create(AID_HEX);
        final Object applet;

        Card() throws Exception {
            TearHook.reset();
            sim.installApplet(aid, cls());
            applet = rt.appletAt(aid);
            TearHook.attach(applet);
            select();
        }

        void select() {
            assertEquals(0x9000, sim.transmitCommand(new CommandAPDU(0x00, 0xA4, 0x04, 0x00, Bip340.hex(AID_HEX))).getSW());
        }

        ResponseAPDU tx(int ins, int p1, byte[] data) {
            TearHook.refresh();
            ResponseAPDU r = sim.transmitCommand(data == null ? new CommandAPDU(CLA, ins, p1, 0) : new CommandAPDU(CLA, ins, p1, 0, data));
            if (TearHook.depth > 0 && TearHook.tornAt == null) {
                // a command that left process() with a transaction open: a chip rolls it back
                TearHook.leftOpen.add(String.format("INS %02X answered %04X", ins, r.getSW()));
                TearHook.rollback();
                rt.clearTx();
            }
            return r;
        }

        void powerCycle() {
            if (TearHook.tornInTx) TearHook.rollback();
            TearHook.dead = false;
            TearHook.depth = 0;
            sim.reset();
            rt.clearTx();
            select();
        }

        Object field(String name) throws ReflectiveOperationException {
            java.lang.reflect.Field f = applet.getClass().getDeclaredField(name);
            f.setAccessible(true);
            return f.get(applet);
        }

        int slotState() throws ReflectiveOperationException { return ((byte[]) field("state"))[0]; }

        byte[] storedSig() throws ReflectiveOperationException { return Arrays.copyOf((byte[]) field("spentSig"), 64); }
    }

    /** P-256's generator: a registry key for tests that never check a registry signature. */
    static final byte[] TEST_REGISTRY_KEY = CardTestSupport.TEST_REGISTRY_KEY;

    /** SET_PIN as provisioning sends it: the registry key first (SET_PIN takes only a card with one), the anchor, then the PIN. */
    static void setPin(Card c) {
        assertEquals(0x9000, c.tx(0x34, 0, TEST_REGISTRY_KEY).getSW());
        assertEquals(0x9000, c.tx(0x19, 0, Headers.anchor()).getSW(), "the set-up's anchor");
        assertEquals(0x9000, c.tx(SET_PIN, 0, PIN).getSW());
    }

    /** A keyed note under the PIN, in a session that has the PIN: SIGN's set-up. Returns the slot's x-only key. */
    static byte[] readyNote(Card c) {
        ResponseAPDU k = c.tx(GEN_KEY, 0, null);
        assertEquals(0x9000, k.getSW());
        byte[] note = new byte[56];
        note[7] = 0x40;
        for (int i = 8; i < 56; i++) note[i] = (byte) (0xA0 + i);
        setPin(c);
        assertEquals(0x9000, c.tx(VERIFY_PIN, 0, PIN).getSW());
        assertEquals(0x9000, c.tx(SET_NOTE, 0, Registry.signed(k.getData(), note)).getSW());
        return k.getData();
    }

    /** Whether the card holds the note or the spend, and answers SIGN as it then must. Empty when it does. */
    static List<String> holds(Card c, byte[] pubX) throws Exception {
        List<String> wrong = new ArrayList<>();
        assertEquals(0x9000, c.tx(VERIFY_PIN, 0, PIN).getSW());
        int st = c.slotState();
        byte[] stored = c.storedSig();
        if (st == ST_NOTE) {
            if (!Arrays.equals(new byte[64], stored)) wrong.add("a note with a signature stored");
            ResponseAPDU r = c.tx(SIGN, 0, MSG);
            if (r.getSW() != 0x9000 || !Bip340.verify(pubX, MSG, r.getData())) wrong.add("the note does not sign: " + Integer.toHexString(r.getSW()));
        } else if (st == ST_SPENT) {
            if (!Bip340.verify(pubX, MSG, stored)) wrong.add("SPENT with a signature that does not verify");
            ResponseAPDU r = c.tx(SIGN, 0, MSG);
            if (r.getSW() != 0x9000 || !Arrays.equals(stored, r.getData())) wrong.add("SPENT, but the stored signature is not answered");
        } else {
            wrong.add("slot state " + st);
        }
        return wrong;
    }

    @Test
    void signTornAfterEachWriteHoldsTheNoteOrTheSpend() throws Exception {
        // the untorn run, counted and traced
        Card c = new Card();
        byte[] pubX = readyNote(c);
        TearHook.count = 0;
        TearHook.target = 0;
        TearHook.trace = new ArrayList<>();
        int nonAtomicBefore = TearHook.nonAtomicInTx.size(), leftOpenBefore = TearHook.leftOpen.size();
        assertEquals(0x9000, c.tx(SIGN, 0, MSG).getSW());
        int total = TearHook.count;
        List<String> trace = TearHook.trace;
        TearHook.trace = null;
        String all = String.join("\n", trace);
        assertTrue(total >= 6, "SIGN writes the hash, the signature, the state and the clock, then commits:\n" + all);
        for (String w : new String[] { "spentHash", "spentSig", "state[0]", "clock", "commit @sign:" }) {
            assertTrue(all.contains(w), "the trace shows " + w + ":\n" + all);
        }
        assertFalse(all.contains("[tx] arrayCopyNonAtomic"), "no NonAtomic write inside SIGN's transaction:\n" + all);
        assertEquals(trace.size(), total);
        c.powerCycle();
        assertEquals(List.of(), holds(c, pubX));
        assertEquals(ST_SPENT, c.slotState());

        List<String> wrong = new ArrayList<>();
        int notes = 0, spends = 0;
        for (int k = 1; k <= total; k++) {
            Card t = new Card();
            byte[] px = readyNote(t);
            TearHook.count = 0;
            TearHook.target = k;
            ResponseAPDU r = t.tx(SIGN, 0, MSG);
            String at = TearHook.tornAt;
            TearHook.target = 0;
            // the harness tears where it is told: after write k of the counted run, and the answer is not a signature
            if (at == null) { wrong.add("write " + k + " was not reached"); continue; }
            String told = trace.get(k - 1);
            String what = at.replace(" (in a transaction)", "");
            if (!told.endsWith(what)) wrong.add("torn at " + at + ", told " + told);
            if (r.getSW() == 0x9000) wrong.add("write " + k + ": a torn SIGN answered 9000");
            t.powerCycle();
            int st = t.slotState();
            if (st == ST_NOTE) notes++;
            if (st == ST_SPENT) spends++;
            for (String w : holds(t, px)) wrong.add("write " + k + " (" + at + "): " + w);
        }
        System.out.println("SIGN tear sweep: " + total + " writes, each torn: " + notes + " left the note, " + spends + " the spend");
        assertEquals(List.of(), wrong);
        assertTrue(notes > 0 && spends > 0, "the sweep reaches both sides of the commit: " + notes + " notes, " + spends + " spends");
        assertEquals(List.of(), TearHook.nonAtomicInTx.subList(nonAtomicBefore, TearHook.nonAtomicInTx.size()));
        assertEquals(List.of(), TearHook.leftOpen.subList(leftOpenBefore, TearHook.leftOpen.size()));
    }

    @Test
    void afterATearNoKeyWriteNorFieldStoreLands() throws Exception {
        // GEN_KEY, untorn and traced: where its key generation comes
        Card c = new Card();
        setPin(c);
        TearHook.count = 0;
        TearHook.target = 0;
        TearHook.trace = new ArrayList<>();
        assertEquals(0x9000, c.tx(GEN_KEY, 0, null).getSW());
        List<String> trace = TearHook.trace;
        TearHook.trace = null;
        int k = 0;
        for (int i = 0; i < trace.size() && k == 0; i++) if (trace.get(i).contains(" genKeyPair")) k = i + 1;
        assertTrue(k > 0, "GEN_KEY generates its key:\n" + String.join("\n", trace));
        assertFalse(((KeyPair) c.field("gen")).getPrivate().isInitialized(), "untorn, GEN_KEY's finally clears the generated key");

        // torn there: the power is gone, so the finally's clearKey and domain parameters never reach the key object
        Card t = new Card();
        setPin(t);
        TearHook.count = 0;
        TearHook.target = k;
        t.tx(GEN_KEY, 0, null);
        TearHook.target = 0;
        assertNotNull(TearHook.tornAt);
        assertTrue(TearHook.tornAt.startsWith("genKeyPair"), TearHook.tornAt);
        assertTrue(TearHook.dead);
        assertTrue(((KeyPair) t.field("gen")).getPrivate().isInitialized(), "the generated key is as the tear left it");

        // nor does a store into one of the applet's fields, until the power comes back
        byte before = (byte) t.field("pinState");
        TearHook.putField(t.applet, before ^ 0x7F, "pinState");
        assertEquals(before, (byte) t.field("pinState"));
        t.powerCycle();
        assertFalse(TearHook.dead);
        TearHook.putField(t.applet, before, "pinState");
        assertEquals(before, (byte) t.field("pinState"));
    }

    @Test
    void aTearInsideTheTransactionRollsItAllBack() throws Exception {
        // Torn on the transaction's last write, after it has written the hash, the signature, the state and the
        // clock: every one of them is as it was before the SIGN. With rollback switched off this fails.
        Card c = new Card();
        readyNote(c);
        TearHook.count = 0;
        TearHook.trace = new ArrayList<>();
        c.tx(SIGN, 0, MSG);
        int last = 0;
        for (int i = 0; i < TearHook.trace.size(); i++) if (TearHook.trace.get(i).contains(" [tx] ")) last = i + 1;
        String lastWrite = last > 0 ? TearHook.trace.get(last - 1) : "";
        TearHook.trace = null;
        assertTrue(last > 1, "SIGN writes inside a transaction");
        assertTrue(lastWrite.contains("clock"), "the last write in the transaction is the clock: " + lastWrite);

        Card t = new Card();
        readyNote(t);
        byte[] state = ((byte[]) t.field("state")).clone(), clock = ((byte[]) t.field("clock")).clone();
        byte[] hash = ((byte[]) t.field("spentHash")).clone();
        TearHook.count = 0;
        TearHook.target = last;
        t.tx(SIGN, 0, MSG);
        TearHook.target = 0;
        assertTrue(TearHook.tornInTx, "torn inside the transaction: " + TearHook.tornAt);
        t.powerCycle();
        assertArrayEquals(state, (byte[]) t.field("state"));
        assertArrayEquals(clock, (byte[]) t.field("clock"));
        assertArrayEquals(hash, (byte[]) t.field("spentHash"));
        assertArrayEquals(new byte[64], t.storedSig());
    }
}
