package io.github.bobodread876.fedimintcard.applet;

import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

import java.math.BigInteger;
import java.util.Random;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * Math-level tests for SchnorrHW's hand-rolled 256-bit modular arithmetic.
 *
 * These deliberately bypass the card/APDU layer and check the primitives
 * directly against java.math.BigInteger, so that a failing BIP-340 signature
 * can be attributed to either the modular arithmetic or the higher-level
 * signing logic rather than guessed at.
 */
class SchnorrHWMathTest {

    /** secp256k1 group order n. */
    private static final BigInteger N = new BigInteger(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141", 16);

    private static byte[] to32(BigInteger v) {
        byte[] out = new byte[32];
        byte[] raw = v.toByteArray();
        int len = Math.min(raw.length, 32);
        System.arraycopy(raw, raw.length - len, out, 32 - len, len);
        return out;
    }

    private static BigInteger from32(byte[] b) {
        return new BigInteger(1, b);
    }

    /** Deterministic seed so failures are reproducible. */
    private static Random rng() {
        return new Random(0xCA54D00DL);
    }

    /**
     * Scratch buffer for mulModN. On-card this is a CLEAR_ON_DESELECT transient
     * array allocated once in the SchnorrHW constructor; here a plain array is
     * fine. Deliberately sized from the constant so a future growth of the
     * arithmetic scratch shows up here rather than as an out-of-bounds read.
     */
    private static byte[] work() {
        return new byte[SchnorrHW.WORK_LEN];
    }

    @Test
    @DisplayName("addModN matches BigInteger (a + b) mod n")
    void addModNMatchesBigInteger() {
        Random r = rng();
        for (int i = 0; i < 200; i++) {
            BigInteger a = new BigInteger(256, r).mod(N);
            BigInteger b = new BigInteger(256, r).mod(N);
            byte[] out = new byte[32];
            SchnorrHW.addModN(to32(a), (short) 0, to32(b), (short) 0, out, (short) 0);
            assertEquals(a.add(b).mod(N), from32(out),
                "addModN mismatch at iteration " + i + " (a=" + a.toString(16)
                    + ", b=" + b.toString(16) + ")");
        }
    }

    @Test
    @DisplayName("mulModN matches BigInteger (a * b) mod n")
    void mulModNMatchesBigInteger() {
        Random r = rng();
        for (int i = 0; i < 200; i++) {
            BigInteger a = new BigInteger(256, r).mod(N);
            BigInteger b = new BigInteger(256, r).mod(N);
            byte[] out = new byte[32];
            SchnorrHW.mulModN(to32(a), (short) 0, to32(b), (short) 0, out, (short) 0,
                work(), (short) 0);
            assertEquals(a.multiply(b).mod(N), from32(out),
                "mulModN mismatch at iteration " + i + " (a=" + a.toString(16)
                    + ", b=" + b.toString(16) + ")");
        }
    }

    @Test
    @DisplayName("mulModN handles edge values (0, 1, n-1)")
    void mulModNEdgeCases() {
        BigInteger[] vals = { BigInteger.ZERO, BigInteger.ONE, N.subtract(BigInteger.ONE),
                              BigInteger.TWO, N.shiftRight(1) };
        for (BigInteger a : vals) {
            for (BigInteger b : vals) {
                byte[] out = new byte[32];
                SchnorrHW.mulModN(to32(a), (short) 0, to32(b), (short) 0, out, (short) 0,
                    work(), (short) 0);
                assertEquals(a.multiply(b).mod(N), from32(out),
                    "mulModN edge mismatch a=" + a.toString(16) + " b=" + b.toString(16));
            }
        }
    }

    // ── D21: the field multiply does the same work for every input ───────────

    /** An operand with each byte zeroed with probability `zeros` in 1: the inputs an early exit on a zero byte would run faster on. */
    private static byte[] sparse(Random r, int zeros) {
        byte[] b = new byte[32];
        r.nextBytes(b);
        for (int k = 0; k < 32; k++) if (r.nextInt(zeros) == 0) b[k] = 0;
        return b;
    }

    @Test
    @DisplayName("D21: mulModN matches BigInteger on operands full of zero bytes, a zero high or low half, and single-byte operands")
    void mulModNWithZeroBytesMatchesBigInteger() {
        Random r = new Random(0x0D21L);
        for (int i = 0; i < 2000; i++) {
            byte[] a = sparse(r, 1 + (i % 4)), b = sparse(r, 1 + ((i / 4) % 4));
            if (i % 50 == 0) java.util.Arrays.fill(a, 0, 16, (byte) 0);       // a zero high half
            if (i % 50 == 1) java.util.Arrays.fill(b, 16, 32, (byte) 0);      // a zero low half
            if (i % 50 == 2) { java.util.Arrays.fill(a, (byte) 0); a[r.nextInt(32)] = (byte) (1 + r.nextInt(255)); }
            BigInteger av = from32(a).mod(N), bv = from32(b).mod(N);
            byte[] out = new byte[32];
            SchnorrHW.mulModN(to32(av), (short) 0, to32(bv), (short) 0, out, (short) 0, work(), (short) 0);
            assertEquals(av.multiply(bv).mod(N), from32(out), "iteration " + i + " a=" + av.toString(16) + " b=" + bv.toString(16));
        }
    }

    @Test
    @DisplayName("D21: the raw 512-bit products, mul256x256 and mulByDelta, match BigInteger with zero bytes in the secret operand")
    void theRawProductsMatchBigInteger() throws Exception {
        java.lang.reflect.Method mul = SchnorrHW.class.getDeclaredMethod("mul256x256",
            byte[].class, short.class, byte[].class, short.class, byte[].class, short.class);
        // mulByDelta works the rows from its last argument on (0: all of them)
        java.lang.reflect.Method byDelta = SchnorrHW.class.getDeclaredMethod("mulByDelta",
            byte[].class, short.class, byte[].class, short.class, short.class);
        mul.setAccessible(true);
        byDelta.setAccessible(true);
        BigInteger delta = BigInteger.ONE.shiftLeft(256).subtract(N);
        Random r = new Random(0x0D21DL);
        for (int i = 0; i < 2000; i++) {
            byte[] a = sparse(r, 1 + (i % 5)), b = sparse(r, 1 + ((i / 5) % 5));
            if (i == 0) java.util.Arrays.fill(a, (byte) 0);
            if (i == 1) java.util.Arrays.fill(a, (byte) 0xFF);
            if (i == 2) { java.util.Arrays.fill(a, (byte) 0xFF); java.util.Arrays.fill(b, (byte) 0xFF); }
            byte[] out = new byte[64];
            java.util.Arrays.fill(out, (byte) 0x5A);    // whatever was there before is written over
            mul.invoke(null, a, (short) 0, b, (short) 0, out, (short) 0);
            assertEquals(from32(a).multiply(from32(b)), new BigInteger(1, out), "a×b at " + i);
            byte[] d = new byte[64];
            java.util.Arrays.fill(d, (byte) 0xA5);
            byDelta.invoke(null, a, (short) 0, d, (short) 0, (short) 0);
            assertEquals(from32(a).multiply(delta), new BigInteger(1, d), "a×DELTA at " + i);
        }
    }

    @Test
    @DisplayName("D21: neither multiply has a branch or a loop on a byte of its operands (no continue, no if, no while), read in the source")
    void theMultipliesHaveNoSecretDependentBranch() throws Exception {
        String src = new String(java.nio.file.Files.readAllBytes(mainSourceDir().resolve("SchnorrHW.java")),
            java.nio.charset.StandardCharsets.UTF_8);
        java.util.List<String> violations = branchyMultiplies(src);
        org.junit.jupiter.api.Assertions.assertTrue(violations.isEmpty(), String.join("\n", violations));
    }

    @Test
    @DisplayName("D21: the scan fails the old early exit, the old carry loop and an if on an operand byte")
    void theBranchScanCanSayNo() {
        String rest = " private static void mulByDelta(byte[] a, short aOff, byte[] out, short outOff) {"
            + " for (short i = 31; i >= 0; i--) { short ai = (short)(a[i] & 0xFF); short carry = 0;"
            + " for (short j = 31; j >= DELTA_FROM; j--) { carry = 0; } out[i] = (byte) carry; } }";
        String good = "class A { private static void mul256x256(byte[] a, short aOff, byte[] b, short bOff, byte[] out, short outOff) {"
            + " for (short i = 31; i >= 0; i--) { short ai = (short)(a[i] & 0xFF); short carry = 0;"
            + " for (short j = 31; j >= 0; j--) { carry = 0; } out[i] = (byte) carry; } }" + rest + " }";
        String earlyExit = good.replace("short ai = (short)(a[i] & 0xFF);", "short ai = (short)(a[i] & 0xFF); if (ai == 0) continue;");
        String carryLoop = good.replace("out[i] = (byte) carry; } }" + rest, "while (carry != 0 && pos >= outOff) { carry = 0; } } }" + rest);
        String anIf = good.replace(rest, rest.replace("short carry = 0;", "short carry = 0; if (ai > 1) carry = 1;"));
        org.junit.jupiter.api.Assertions.assertTrue(branchyMultiplies(good).isEmpty(), String.join("\n", branchyMultiplies(good)));
        org.junit.jupiter.api.Assertions.assertFalse(branchyMultiplies(earlyExit).isEmpty(), "the early exit passed");
        org.junit.jupiter.api.Assertions.assertFalse(branchyMultiplies(carryLoop).isEmpty(), "the carry loop passed");
        org.junit.jupiter.api.Assertions.assertFalse(branchyMultiplies(anIf).isEmpty(), "an if in mulByDelta passed");
    }

    /**
     * D21's reach. D21 took the branches out of the two multiplies; the reductions on the same signing path (e·d, k + e·d, k)
     * branched on values derived from the key and the nonce, and were listed as open. They are branch-free now, held by
     * branchyReductions below. This holds the documents to that: no document lists a reduction as still branching, and neither
     * the spec nor the threat table claims more than the arithmetic in bytecode (the VM's own timing and the chip's engines are
     * not covered, and nothing is measured on silicon).
     */
    static final String[] FORMERLY_BRANCHING = { "reduce512toModN", "addModN", "reduceModN", "sub32", "cmp32", "mulSmallByDelta" };



    /** `continue`, `if`, `while`, `break`, `?` or `return` in the bodies of mul256x256 and mulByDelta: each a branch whose timing could follow a byte of the operands. */
    static java.util.List<String> branchyMultiplies(String rawSrc) {
        String src = stripCommentsAndCharLiterals(rawSrc);
        java.util.List<String> out = new java.util.ArrayList<>();
        for (String name : new String[] { "mul256x256(byte[]", "mulByDelta(byte[]" }) {
            int decl = src.indexOf("static void " + name);
            if (decl < 0) { out.add("no " + name + " in the source"); continue; }
            int[] body = bodyRange(src, "static void " + name);
            String b = src.substring(body[0], body[1]);
            for (String w : new String[] { "continue", "if", "while", "break", "return" }) {
                if (java.util.regex.Pattern.compile("\\b" + w + "\\b").matcher(b).find()) out.add(name + " has `" + w + "`");
            }
            if (b.contains("?")) out.add(name + " has a conditional expression");
        }
        return out;
    }

    /**
     * The odd-y normalisation d = n - d is picked at random by the card's key
     * generation (P.y parity), so the applet-level tests only exercise it about
     * half the time on any given run. This pins the primitive deterministically.
     */
    @Test
    @DisplayName("subtractFromN matches BigInteger n - a")
    void subtractFromNMatchesBigInteger() {
        Random r = rng();
        for (int i = 0; i < 200; i++) {
            BigInteger a = new BigInteger(256, r).mod(N);
            byte[] out = new byte[32];
            SchnorrHW.subtractFromN(to32(a), (short) 0, out, (short) 0);
            assertEquals(N.subtract(a), from32(out),
                "subtractFromN mismatch at iteration " + i + " (a=" + a.toString(16) + ")");
        }
    }

    @Test
    @DisplayName("subtractFromN handles edge values (0, 1, n-1)")
    void subtractFromNEdgeCases() {
        BigInteger[] vals = { BigInteger.ZERO, BigInteger.ONE, BigInteger.TWO,
                              N.subtract(BigInteger.ONE), N.shiftRight(1) };
        for (BigInteger a : vals) {
            byte[] out = new byte[32];
            SchnorrHW.subtractFromN(to32(a), (short) 0, out, (short) 0);
            assertEquals(N.subtract(a), from32(out),
                "subtractFromN edge mismatch a=" + a.toString(16));
        }
    }

    /**
     * Reusing one scratch buffer across calls must not leak state between them:
     * on-card the buffer is a single long-lived transient array, so a helper that
     * reads a stale byte instead of writing it first would only misbehave on the
     * second and later signatures.
     */
    @Test
    @DisplayName("mulModN is correct when the work buffer is reused and pre-dirtied")
    void mulModNReusesWorkBufferSafely() {
        Random r = rng();
        byte[] shared = work();
        java.util.Arrays.fill(shared, (byte) 0xA5);
        for (int i = 0; i < 100; i++) {
            BigInteger a = new BigInteger(256, r).mod(N);
            BigInteger b = new BigInteger(256, r).mod(N);
            byte[] out = new byte[32];
            SchnorrHW.mulModN(to32(a), (short) 0, to32(b), (short) 0, out, (short) 0,
                shared, (short) 0);
            assertEquals(a.multiply(b).mod(N), from32(out),
                "mulModN mismatch on reused work buffer at iteration " + i);
        }
    }

    /**
     * The methods that are allowed to allocate, per source file: everything that
     * runs exactly once at install time. Anything else in these files is
     * reachable from an APDU handler.
     *
     * Entry format: { file name, install-time declaration, ... }. The
     * declarations are matched literally against the comment-stripped source, so
     * they must be unique in it.
     *
     * Every .java file under src/main/java/me/flashapp/cashu must appear here;
     * a new one with no entry fails the test rather than slipping through
     * unchecked.
     *
     * "Runs exactly once at install time" is itself enforced, not taken on
     * trust: {@link #assertNoAllocationOutside} requires every unqualified call
     * to a listed method to sit inside another listed method's body. A helper
     * like {@code initCardKeypair()} is only install-time for as long as nobody
     * wires it to an APDU handler, and a whole-body allocation waiver granted on
     * the strength of a comment is exactly how the leak this test guards against
     * would come back.
     */
    private static final String[][] INSTALL_TIME_METHODS = {
        { "SchnorrHW.java",
          "SchnorrHW(", "void init()" },
        { "FedimintCardApplet.java",
          "public static void install(", "private FedimintCardApplet()" },
        { "Opener.java",
          "public static void install(", "private Opener()" },
        { "Secp256k1.java" },
    };

    /**
     * The allocations that are not install-time and are let through all the same (1.20), each by its file, by the method it is
     * in and by the whole statement it is, so that nothing else in that method, or anywhere else, goes through with it:
     * { file, the method's declaration, the statement }. The PIN's work keys, made at the first SET_PIN behind a null
     * check, are the one registered here.
     */
    private static final String[][] ONCE_ONLY_ALLOCATIONS = {
        { "FedimintCardApplet.java", "private void setPin(", "workKeys = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);" },
    };

    /**
     * JavaCard Classic allocates `new` in persistent EEPROM/Flash and never
     * collects it, so a single `new byte[]` on an APDU path leaks the card's
     * persistent memory a few dozen bytes per tap until every write command
     * throws. jCardSim runs on the JVM heap with a real GC and therefore cannot
     * surface this — only a source-level check can. It is CONTRIBUTING.md
     * principle 5, and this test is its enforcement.
     *
     * Both applet sources are scanned, not just the signer: {@code CashuApplet}
     * is every bit as much on the APDU path, and a stray {@code byte[] tmp = new
     * byte[64]} inside processLoadProof would leak exactly the same way.
     */
    @Test
    @DisplayName("no applet source allocates outside its install-time methods")
    void noAllocationOutsideInstallTime() throws Exception {
        java.nio.file.Path dir = mainSourceDir();
        java.util.List<java.nio.file.Path> sources;
        try (java.util.stream.Stream<java.nio.file.Path> s = java.nio.file.Files.list(dir)) {
            sources = s.filter(p -> p.getFileName().toString().endsWith(".java"))
                       .sorted()
                       .collect(java.util.stream.Collectors.toList());
        }
        org.junit.jupiter.api.Assertions.assertFalse(sources.isEmpty(),
            "no applet sources found under " + dir.toAbsolutePath());

        java.util.Set<String> scanned = new java.util.HashSet<>();
        for (java.nio.file.Path p : sources) {
            String name = p.getFileName().toString();
            String[] installTime = installTimeMethods(name);
            assertNoAllocationOutside(name,
                new String(java.nio.file.Files.readAllBytes(p),
                    java.nio.charset.StandardCharsets.UTF_8),
                installTime, onceOnlyAllocations(name));
            scanned.add(name);
        }

        for (String[] entry : INSTALL_TIME_METHODS) {
            org.junit.jupiter.api.Assertions.assertTrue(scanned.contains(entry[0]),
                "INSTALL_TIME_METHODS names " + entry[0] + ", which no longer exists under "
                + dir.toAbsolutePath() + " — the allow-list is stale");
        }
    }

    /**
     * The guard above is only worth anything if it can say no, so it is pointed
     * at sources written to fail it.
     *
     * The waived-helper leak is the one this exists for: {@code initThing()}
     * allocates and is allow-listed, which is fine while only the constructor
     * calls it. The moment an APDU handler calls it too, every tap allocates a
     * new array in EEPROM — and the allocation itself never moved, so the plain
     * allocation scan stays green. Only the call-site check catches it.
     */
    @Test
    @DisplayName("the allow-list rejects an install-time helper called from the APDU path")
    void allowListRejectsInstallTimeHelperCalledFromApduPath() {
        String[] allowList = {
            "public static void install(", "private Fake()", "private void initThing()"
        };

        String clean =
            "class Fake {\n"
            + "    private byte[] buf;\n"
            + "    public static void install(byte[] a, short b, byte c) { new Fake(); }\n"
            + "    private Fake() { initThing(); }\n"
            + "    private void initThing() { buf = new byte[64]; }\n"
            + "    private void processApdu() { buf[0] = 1; }\n"
            + "}\n";
        assertNoAllocationOutside("Fake.java", clean, allowList);

        String leaky = clean.replace(
            "private void processApdu() { buf[0] = 1; }",
            "private void processApdu() { initThing(); }");
        org.opentest4j.AssertionFailedError failed =
            org.junit.jupiter.api.Assertions.assertThrows(
                org.opentest4j.AssertionFailedError.class,
                () -> assertNoAllocationOutside("Fake.java", leaky, allowList),
                "an allow-listed allocating helper reached from the APDU path must fail");
        org.junit.jupiter.api.Assertions.assertTrue(failed.getMessage().contains("initThing"),
            "the failure must name the helper that gained an APDU-path caller, got: "
                + failed.getMessage());
    }

    /**
     * A call on another object that merely shares a name with an allow-listed
     * method is not a call to it — {@code ecdh.init(tmpPriv)} in SchnorrHW is
     * exactly that, and flagging it would make the guard unusable.
     */
    @Test
    @DisplayName("the call-site check ignores calls qualified by a receiver")
    void allowListIgnoresQualifiedSameNameCalls() {
        String src =
            "class Fake {\n"
            + "    private byte[] buf;\n"
            + "    private Other other;\n"
            + "    public static void install(byte[] a, short b, byte c) { new Fake(); }\n"
            + "    private Fake() { initThing(); }\n"
            + "    private void initThing() { buf = new byte[64]; }\n"
            + "    private void processApdu() { other.initThing(); }\n"
            + "}\n";
        assertNoAllocationOutside("Fake.java", src, new String[] {
            "public static void install(", "private Fake()", "private void initThing()"
        });
    }

    /**
     * The once-only waiver (1.20) is as narrow as it says: the statement, whole, once, in the method it was named for, and nothing beside
     * it. A guard left off (it would allocate again at every SET_CARD), a second allocation next to it, a second copy of it, the same
     * statement in another method, and a waiver for a statement that is not there are each refused.
     */
    @Test
    @DisplayName("the once-only allocation is let through whole, once, in its own method, and nothing beside it: no guard, a second allocation, a second copy, another method and a missing statement are each refused")
    void theOnceOnlyAllocationIsNarrow() {
        String statement = "if (dleq == null) { try { dleq = new byte[96]; } catch (SystemException e) { dleq = null; } }";
        String[] allowList = { "public static void install(", "private Fake()" };
        String[][] once = { { "private void setCard(", statement } };
        String clean =
            "class Fake {\n"
            + "    private byte[] dleq;\n"
            + "    public static void install(byte[] a, short b, byte c) { new Fake(); }\n"
            + "    private Fake() { }\n"
            + "    private void setCard() {\n"
            + "        " + statement + "\n"
            + "    }\n"
            + "    private void other() { }\n"
            + "}\n";
        assertNoAllocationOutside("Fake.java", clean, allowList, once);
        // laid out another way: it is the statement that is let through, and not its spacing
        assertNoAllocationOutside("Fake.java",
            clean.replace(statement, "if (dleq == null) {\n    try {\n        dleq = new byte[96];\n    } catch (SystemException e) {\n        dleq = null;\n    }\n}"),
            allowList, once);
        // an allocation in the constructor is install-time, as ever
        assertNoAllocationOutside("Fake.java", clean.replace("private Fake() { }", "private Fake() { dleq = new byte[4]; }"), allowList, once);

        String[][] refused = {
            // { what is wrong, the source, the part of the failure's message that says so }
            { "no guard", clean.replace(statement, "dleq = new byte[96];"), "is not there, whole" },
            { "guarded by the wrong test", clean.replace("dleq == null", "dleq != null"), "is not there, whole" },
            { "a second allocation beside it", clean.replace(statement, statement + " byte[] t = new byte[4];"), "allocation outside the install-time methods" },
            { "a second copy of it", clean.replace("private void other() { }", "private void other() { " + statement + " }"), "twice" },
            { "the same statement in another method", clean.replace(statement, "").replace("private void other() { }", "private void other() { " + statement + " }"), "elsewhere" },
            { "a waiver for a statement that is not there", clean.replace(statement, ""), "is not there, whole" },
        };
        for (String[] r : refused) {
            org.opentest4j.AssertionFailedError failed = org.junit.jupiter.api.Assertions.assertThrows(org.opentest4j.AssertionFailedError.class,
                () -> assertNoAllocationOutside("Fake.java", r[1], allowList, once), r[0] + " must fail");
            org.junit.jupiter.api.Assertions.assertTrue(failed.getMessage().contains(r[2]), r[0] + ": the failure must say '" + r[2] + "', got: " + failed.getMessage());
        }
        // and without the waiver the same allocation is refused outright, as every allocation on the APDU path always was
        org.junit.jupiter.api.Assertions.assertThrows(org.opentest4j.AssertionFailedError.class,
            () -> assertNoAllocationOutside("Fake.java", clean, allowList), "no waiver, no allocation");
    }

    /** The once-only allocations registered for a source file: { the method's declaration, the statement } each. */
    private static String[][] onceOnlyAllocations(String fileName) {
        java.util.List<String[]> found = new java.util.ArrayList<>();
        for (String[] entry : ONCE_ONLY_ALLOCATIONS) {
            if (entry[0].equals(fileName)) found.add(new String[] { entry[1], entry[2] });
        }
        return found.toArray(new String[0][]);
    }

    /** The install-time declarations registered for a source file. */
    private static String[] installTimeMethods(String fileName) {
        for (String[] entry : INSTALL_TIME_METHODS) {
            if (entry[0].equals(fileName)) {
                return java.util.Arrays.copyOfRange(entry, 1, entry.length);
            }
        }
        throw new org.opentest4j.AssertionFailedError(
            fileName + " is on the APDU path but has no entry in INSTALL_TIME_METHODS. "
            + "Add one listing the methods that run once at install time (constructor, "
            + "install(), anything called only from them) so the rest of the file is "
            + "checked for persistent-memory allocation.");
    }

    /**
     * Fails if the source allocates anywhere inside a method body other than the
     * given install-time declarations.
     */
    private static void assertNoAllocationOutside(String fileName, String rawSrc,
                                                  String[] installTimeDeclarations) {
        assertNoAllocationOutside(fileName, rawSrc, installTimeDeclarations, new String[0][]);
    }

    /**
     * The same, and the once-only statements ({ method declaration, statement }) let through: each must be there, once, whole, inside
     * the body of the method named.
     */
    private static void assertNoAllocationOutside(String fileName, String rawSrc,
                                                  String[] installTimeDeclarations, String[][] onceOnly) {
        String src = stripCommentsAndCharLiterals(rawSrc);

        int[][] allowedRanges = new int[installTimeDeclarations.length + onceOnly.length][];
        for (int i = 0; i < installTimeDeclarations.length; i++) {
            allowedRanges[i] = bodyRange(src, installTimeDeclarations[i]);
        }
        for (int i = 0; i < onceOnly.length; i++) {
            int[] method = bodyRange(src, onceOnly[i][0]);
            int[] at = findIgnoringSpace(src, onceOnly[i][1], 0, src.length());
            org.junit.jupiter.api.Assertions.assertNotNull(at,
                "the once-only allocation of " + fileName + " is not there, whole: " + onceOnly[i][1]);
            org.junit.jupiter.api.Assertions.assertTrue(at[0] > method[0] && at[1] < method[1],
                "the once-only allocation of " + fileName + " is not in " + onceOnly[i][0] + " but elsewhere: " + onceOnly[i][1]);
            org.junit.jupiter.api.Assertions.assertNull(findIgnoringSpace(src, onceOnly[i][1], at[1], src.length()),
                "the once-only allocation of " + fileName + " is there twice: " + onceOnly[i][1]);
            allowedRanges[installTimeDeclarations.length + i] = new int[] { at[0] - 1, at[1] };
        }

        // a once-only statement is no install-time body: an install-time helper called from it would be called from the APDU path
        assertOnlyCalledAtInstallTime(fileName, src, installTimeDeclarations,
            java.util.Arrays.copyOf(allowedRanges, installTimeDeclarations.length));

        // `new byte[..]` plus the array-initializer form `byte[] x = { .. }`,
        // which allocates just the same but contains no `new` keyword.
        java.util.regex.Pattern allocation = java.util.regex.Pattern.compile(
            "\\bnew\\b"
            + "|\\b(?:byte|short|int|boolean|Object)\\s*\\[\\s*\\]\\s*\\w+\\s*=\\s*\\{");

        int[] depth = braceDepths(src);

        java.util.regex.Matcher m = allocation.matcher(src);
        while (m.find()) {
            int at = m.start();
            // Depth 1 is the class body: field initialisers run once when the
            // applet is installed, which is exactly what we want. Depth >= 2 is
            // inside a method, and only the install-time methods may allocate.
            boolean allowed = depth[at] < 2;
            for (int[] range : allowedRanges) {
                allowed |= at > range[0] && at < range[1];
            }
            if (!allowed) {
                int from = Math.max(0, at - 90);
                int to = Math.min(src.length(), at + 90);
                org.junit.jupiter.api.Assertions.fail(
                    "allocation outside the install-time methods of " + fileName
                    + " leaks persistent EEPROM on every call — allocate once at install "
                    + "time as a CLEAR_ON_DESELECT transient array and thread it through "
                    + "as (work, workOff). Found near:\n..."
                    + src.substring(from, to).replaceAll("\\s+", " ") + "...");
            }
        }
    }

    /**
     * Fails if a method whose whole body is waived as install-time is reachable
     * from anywhere that is not itself waived.
     *
     * The waiver on {@code initCardKeypair()} is only sound while the "called
     * once, from the constructor" claim in its comment holds. Add a
     * {@code 0x4x REGENERATE_KEY} handler that calls it and the leak is back —
     * a fresh {@code KeyPair} in EEPROM per APDU — with the allocation guard
     * still green, because the allocation never moved. So the claim is checked
     * rather than read: every unqualified call to a listed name must sit inside
     * a listed body.
     *
     * Unqualified on purpose. {@code ecdh.init(tmpPriv)} is a call on another
     * object that happens to share a name with {@code SchnorrHW.init()}, and a
     * receiver before the dot is the one signal available without a type
     * checker. The flip side is the known limit of this check: a cross-file
     * {@code schnorrHW.init()} from an APDU handler is invisible to it. Both
     * cross-file entry points here are install-time by construction — the JCRE
     * calls {@code install()}, which calls the constructor — and the guard
     * covers the case that actually rots, which is a local helper quietly
     * gaining a second caller.
     */
    private static void assertOnlyCalledAtInstallTime(String fileName, String src,
                                                      String[] installTimeDeclarations,
                                                      int[][] allowedRanges) {
        for (int i = 0; i < installTimeDeclarations.length; i++) {
            String declaration = installTimeDeclarations[i];
            String name = declaredName(declaration);
            int declAt = src.indexOf(declaration);
            int declEnd = declAt + declaration.length();

            java.util.regex.Matcher calls = java.util.regex.Pattern
                .compile("(?<![.\\w$])" + java.util.regex.Pattern.quote(name) + "\\s*\\(")
                .matcher(src);
            while (calls.find()) {
                int at = calls.start();
                if (at >= declAt && at < declEnd) {
                    continue;   // the declaration itself
                }
                boolean allowed = false;
                for (int[] range : allowedRanges) {
                    allowed |= at > range[0] && at < range[1];
                }
                if (!allowed) {
                    int from = Math.max(0, at - 90);
                    int to = Math.min(src.length(), at + 90);
                    org.junit.jupiter.api.Assertions.fail(
                        name + "() is on the INSTALL_TIME_METHODS allow-list of " + fileName
                        + ", so its entire body is waived from the no-allocation rule — but "
                        + "it is called from outside every install-time method, i.e. from the "
                        + "APDU path. Either drop the waiver and stop allocating in it, or "
                        + "remove the call. Found near:\n..."
                        + src.substring(from, to).replaceAll("\\s+", " ") + "...");
                }
            }
        }
    }

    /** The method (or constructor) name in a declaration like "private void foo(". */
    private static String declaredName(String declaration) {
        int paren = declaration.indexOf('(');
        if (paren < 0) {
            throw new IllegalStateException(
                "INSTALL_TIME_METHODS entries must name a method: " + declaration);
        }
        int end = paren;
        while (end > 0 && Character.isWhitespace(declaration.charAt(end - 1))) end--;
        int start = end;
        while (start > 0 && Character.isJavaIdentifierPart(declaration.charAt(start - 1))) start--;
        if (start == end) {
            throw new IllegalStateException(
                "no method name found in INSTALL_TIME_METHODS entry: " + declaration);
        }
        return declaration.substring(start, end);
    }

    /** Applet main-source directory, resolved from either module or repo root. */
    static java.nio.file.Path mainSourceDir() throws java.io.IOException {
        String[] candidates = {
            "src/main/java/io/github/bobodread876/fedimintcard/applet",
            "applet/src/main/java/io/github/bobodread876/fedimintcard/applet",
        };
        for (String c : candidates) {
            java.nio.file.Path p = java.nio.file.Paths.get(c);
            if (java.nio.file.Files.isDirectory(p)) {
                return p;
            }
        }
        throw new java.io.FileNotFoundException(
            "applet sources not found relative to "
            + java.nio.file.Paths.get("").toAbsolutePath());
    }

    /** Drop comments and char literals so `new` and braces are only ever real code. */
    static String stripCommentsAndCharLiterals(String s) {
        StringBuilder out = new StringBuilder(s.length());
        int i = 0;
        while (i < s.length()) {
            if (s.startsWith("/*", i)) {
                int e = s.indexOf("*/", i + 2);
                i = (e < 0) ? s.length() : e + 2;
            } else if (s.startsWith("//", i)) {
                int e = s.indexOf('\n', i);
                i = (e < 0) ? s.length() : e;
            } else if (s.charAt(i) == '\'') {
                int j = i + 1;
                while (j < s.length() && s.charAt(j) != '\'') {
                    j += (s.charAt(j) == '\\') ? 2 : 1;
                }
                out.append("' '");
                i = j + 1;
            } else {
                out.append(s.charAt(i));
                i++;
            }
        }
        return out.toString();
    }

    /** Brace nesting depth at every character index. Class body = 1, method body = 2. */
    static int[] braceDepths(String src) {
        int[] depth = new int[src.length()];
        int d = 0;
        for (int i = 0; i < src.length(); i++) {
            char c = src.charAt(i);
            if (c == '{') d++;
            depth[i] = d;
            if (c == '}') d--;
        }
        return depth;
    }

    /**
     * Where, in src from `from` to `to`, a run of characters begins that is `wanted` but for its white space: [start, end), or null.
     */
    private static int[] findIgnoringSpace(String src, String wanted, int from, int to) {
        String w = wanted.replaceAll("\\s+", "");
        for (int i = from; i < to; i++) {
            if (Character.isWhitespace(src.charAt(i))) continue;
            int a = i, b = 0;
            while (a < to && b < w.length()) {
                char c = src.charAt(a);
                if (Character.isWhitespace(c)) { a++; continue; }
                if (c != w.charAt(b)) break;
                a++;
                b++;
            }
            if (b == w.length()) return new int[] { i, a };
        }
        return null;
    }

    /** [openBraceIndex, closeBraceIndex] of the body following the given declaration. */
    static int[] bodyRange(String src, String declaration) {
        int decl = src.indexOf(declaration);
        if (decl < 0) throw new IllegalStateException("not found in source: " + declaration);
        int open = src.indexOf('{', decl);
        if (open < 0) throw new IllegalStateException("no body for: " + declaration);
        int depth = 0;
        for (int i = open; i < src.length(); i++) {
            char c = src.charAt(i);
            if (c == '{') depth++;
            else if (c == '}' && --depth == 0) return new int[] { open, i };
        }
        throw new IllegalStateException("unbalanced braces after: " + declaration);
    }

    // ── The reductions: the edges, and the same work whatever the values ────

    private static final BigInteger TWO_256 = BigInteger.ONE.shiftLeft(256);
    private static final BigInteger DELTA_REF = TWO_256.subtract(N);

    private static byte[] to64(BigInteger v) {
        byte[] out = new byte[64];
        byte[] raw = v.toByteArray();
        int len = Math.min(raw.length, 64);
        System.arraycopy(raw, raw.length - len, out, 64 - len, len);
        return out;
    }

    private static java.lang.reflect.Method hidden(String name, Class<?>... types) throws Exception {
        java.lang.reflect.Method m = SchnorrHW.class.getDeclaredMethod(name, types);
        m.setAccessible(true);
        return m;
    }

    @Test
    @DisplayName("addModN at the edges: sums of n-1, n, n+1 and 2n-2, sums whose low 256 bits are just under, at and over n, and 0 + 0, each against BigInteger")
    void addModNAtTheEdges() {
        BigInteger one = BigInteger.ONE, nm1 = N.subtract(one);
        BigInteger[][] pairs = {
            { BigInteger.ZERO, BigInteger.ZERO }, { nm1, BigInteger.ZERO }, { BigInteger.ZERO, nm1 },
            { nm1, one }, { one, nm1 },                                   // n
            { nm1, BigInteger.valueOf(2) },                               // n + 1
            { N.shiftRight(1), N.shiftRight(1) },                         // n - 1 (n is odd)
            { N.shiftRight(1).add(one), N.shiftRight(1) },                // n
            { N.shiftRight(1).add(one), N.shiftRight(1).add(one) },       // n + 1
            { nm1, nm1 },                                                 // 2n - 2, past 2^256
            { DELTA_REF, nm1 },                                           // 2^256 - 1
            { DELTA_REF.add(one), nm1 },                                  // 2^256: carries, low bits zero
            { N.subtract(DELTA_REF), DELTA_REF },                         // n exactly, from a small and a large
            { N.subtract(DELTA_REF).add(one), DELTA_REF },                // n + 1
            { N.subtract(DELTA_REF).subtract(one), DELTA_REF },           // n - 1
        };
        for (BigInteger[] p : pairs) {
            BigInteger a = p[0], b = p[1];
            org.junit.jupiter.api.Assertions.assertTrue(a.compareTo(N) < 0 && b.compareTo(N) < 0 && a.signum() >= 0 && b.signum() >= 0, "addModN is given numbers under n");
            byte[] out = new byte[32];
            java.util.Arrays.fill(out, (byte) 0x5A);
            SchnorrHW.addModN(to32(a), (short) 0, to32(b), (short) 0, out, (short) 0);
            assertEquals(a.add(b).mod(N), from32(out), "addModN(" + a.toString(16) + ", " + b.toString(16) + ")");
        }
        Random r = rng();
        for (int i = 0; i < 2000; i++) {
            BigInteger a = new BigInteger(256, r).mod(N), b = new BigInteger(256, r).mod(N);
            if (i % 3 == 0) b = N.subtract(a).add(BigInteger.valueOf((i % 7) - 3)).mod(N);   // sums at n, either side
            byte[] out = new byte[40];
            SchnorrHW.addModN(to32(a), (short) 0, to32(b), (short) 0, out, (short) 4);
            assertEquals(a.add(b).mod(N), new BigInteger(1, java.util.Arrays.copyOfRange(out, 4, 36)), "addModN at " + i);
        }
    }

    @Test
    @DisplayName("reduceModN at the edges: 0, 1, n-1, n, n+1, 2^256-1, DELTA and n+DELTA-1, against BigInteger, the answer under n")
    void reduceModNAtTheEdges() {
        BigInteger one = BigInteger.ONE;
        for (BigInteger v : new BigInteger[] { BigInteger.ZERO, one, N.subtract(one), N, N.add(one), TWO_256.subtract(one), DELTA_REF, N.add(DELTA_REF).subtract(one) }) {
            byte[] a = to32(v);
            SchnorrHW.reduceModN(a, (short) 0);
            assertEquals(v.mod(N), from32(a), "reduceModN(" + v.toString(16) + ")");
        }
    }

    @Test
    @DisplayName("reduce512toModN at the edges: n-1, n, n+1, 2n-1, 2^256-1, 2^512-1, q*n + r for r at the edges, and numbers built so that the four-term sum carries and the fold of that carry carries again, each against BigInteger")
    void reduce512AtTheEdges() throws Exception {
        java.lang.reflect.Method m = hidden("reduce512toModN", byte[].class, short.class, byte[].class, short.class, byte[].class, short.class);
        BigInteger one = BigInteger.ONE, nm1 = N.subtract(one), two512 = BigInteger.ONE.shiftLeft(512);
        java.util.List<BigInteger> values = new java.util.ArrayList<>(java.util.Arrays.asList(
            BigInteger.ZERO, nm1, N, N.add(one), N.shiftLeft(1).subtract(one), N.shiftLeft(1), TWO_256.subtract(one), TWO_256, TWO_256.add(nm1),
            two512.subtract(one), nm1.multiply(nm1), two512.subtract(N), two512.subtract(one).subtract(two512.subtract(one).mod(N))));
        BigInteger q = new BigInteger("F3E2D1C0B9A8978675645342312F1E0D0C0B0A09080706050403020100FFEEDD", 16);
        for (BigInteger r : new BigInteger[] { BigInteger.ZERO, one, nm1, N, N.add(one), N.shiftLeft(1).subtract(one) }) values.add(q.multiply(N).add(r));
        // the sum the reduction makes from p's high half, worked here as it is there, and a low half chosen so the sum is
        // 2^256 - 1 or 2^256 - 2 past a multiple of 2^256: its carry folded back as DELTA overflows again
        Random rnd = rng();
        int twice = 0;
        for (int i = 0; i < 400; i++) {
            BigInteger hi = new BigInteger(256, rnd);
            if (i % 4 == 0) hi = TWO_256.subtract(one).subtract(BigInteger.valueOf(i));
            BigInteger t = hi.multiply(DELTA_REF), tHi = t.shiftRight(256), tLo = t.mod(TWO_256);
            BigInteger t2 = tHi.multiply(DELTA_REF), t2Hi = t2.shiftRight(256), t2Lo = t2.mod(TWO_256);
            BigInteger part = t2Hi.multiply(DELTA_REF).add(t2Lo).add(tLo);
            for (int eps = 1; eps <= 2; eps++) {
                BigInteger lo = TWO_256.subtract(BigInteger.valueOf(eps)).subtract(part).mod(TWO_256);
                BigInteger sum = part.add(lo);
                BigInteger carry = sum.shiftRight(256);
                if (carry.signum() > 0 && sum.mod(TWO_256).add(carry.multiply(DELTA_REF)).compareTo(TWO_256) >= 0) twice++;
                values.add(hi.shiftLeft(256).add(lo));
            }
        }
        org.junit.jupiter.api.Assertions.assertTrue(twice > 100, "the fold's own carry was reached " + twice + " times");
        for (BigInteger v : values) {
            byte[] p = to64(v), out = new byte[32], w = work();
            java.util.Arrays.fill(w, (byte) 0x5A);
            m.invoke(null, p, (short) 0, out, (short) 0, w, (short) 0);
            assertEquals(v.mod(N), from32(out), "reduce512toModN(" + v.toString(16) + ")");
        }
    }

    @Test
    @DisplayName("mulSmallByDelta still refuses what does not fit: an operand with any of its first 31 bytes not zero (the rows it does not work) is refused 6F00, and one of a single low byte is a*DELTA")
    void mulSmallByDeltaStillRefusesAnOverflow() throws Exception {
        java.lang.reflect.Method m = hidden("mulSmallByDelta", byte[].class, short.class, byte[].class, short.class, byte[].class, short.class);
        for (int at = 0; at < 31; at++) {
            for (int bit : new int[] { 0x01, 0x80 }) {
                byte[] a = new byte[32];
                a[at] = (byte) bit;
                a[31] = 3;
                try {
                    m.invoke(null, a, (short) 0, new byte[32], (short) 0, new byte[64], (short) 0);
                    org.junit.jupiter.api.Assertions.fail("an operand with byte " + at + " set was taken");
                } catch (java.lang.reflect.InvocationTargetException e) {
                    assertEquals(Secp256k1.SW_CRYPTO_ERROR, ((javacard.framework.ISOException) e.getCause()).getReason(), "byte " + at);
                }
            }
        }
        for (int v : new int[] { 0, 1, 2, 3, 0x7F, 0xFF }) {
            byte[] a = new byte[32], out = new byte[32];
            a[31] = (byte) v;
            m.invoke(null, a, (short) 0, out, (short) 0, new byte[64], (short) 0);
            assertEquals(BigInteger.valueOf(v).multiply(DELTA_REF), from32(out), "a = " + v);
        }
    }

    @Test
    @DisplayName("choose: a kept where take is 0, b taken where it is 1, every byte, at offsets")
    void chooseByAMask() throws Exception {
        java.lang.reflect.Method m = hidden("choose", byte[].class, short.class, byte[].class, short.class, short.class);
        Random r = rng();
        for (int i = 0; i < 50; i++) {
            byte[] a = new byte[40], b = new byte[40];
            r.nextBytes(a); r.nextBytes(b);
            byte[] keep = a.clone();
            m.invoke(null, a, (short) 3, b, (short) 5, (short) 0);
            org.junit.jupiter.api.Assertions.assertArrayEquals(keep, a, "take 0 keeps a");
            m.invoke(null, a, (short) 3, b, (short) 5, (short) 1);
            org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(b, 5, 37), java.util.Arrays.copyOfRange(a, 3, 35), "take 1 takes b");
            org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(keep, 0, 3), java.util.Arrays.copyOfRange(a, 0, 3), "and nothing before");
            org.junit.jupiter.api.Assertions.assertArrayEquals(java.util.Arrays.copyOfRange(keep, 35, 40), java.util.Arrays.copyOfRange(a, 35, 40), "or after");
        }
    }

    /** The arithmetic on the signing path that combines the key and the nonce: every body here is scanned by branchyReductions. */
    static final String[] BRANCH_FREE = { "static void addModN(", "static void reduceModN(", "private static void subIf(",
        "private static short under(", "private static void choose(", "static short sub32(", "static void subtractFromN(", "static void mul256x256(",
        "private static void mulByDelta(", "private static short addDelta(", "private static short add4x32(", "private static void reduce512toModN(",
        "private static void mulSmallByDelta(" };
    /** mulSmallByDelta's one check, which tests a value that is zero for every input the reduction makes: allowed once, as its last statement. */
    static final String OVERFLOW_CHECK = "if (over != 0) ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);";

    /**
     * What, in the bodies of BRANCH_FREE, could make the time of a signature follow a value: `if`, `while`, `do`, `continue`,
     * `break`, `switch`, `?:`, a compare to zero, a `return` before the end, a call of an early-exit compare (`cmp32`, kept for
     * the applet's public values and called nowhere in the signer), and a `for` loop whose bounds are not constants (a length
     * is LEN, the 32 every call gives; mulByDelta's first row is `from`, a constant at each of its three calls).
     * mulSmallByDelta may end with OVERFLOW_CHECK, once.
     */
    static java.util.List<String> branchyReductions(String rawSrc) {
        String src = stripCommentsAndCharLiterals(rawSrc);
        java.util.List<String> out = new java.util.ArrayList<>();
        for (String decl : BRANCH_FREE) {
            if (src.indexOf(decl) < 0) { out.add("no " + decl + " in the source"); continue; }
            int[] r = bodyRange(src, decl);
            String body = src.substring(r[0] + 1, r[1]).replaceAll("\\s+", " ").trim();
            if (decl.equals("private static void mulSmallByDelta(")) {
                if (!body.endsWith(OVERFLOW_CHECK) || body.indexOf(OVERFLOW_CHECK) != body.lastIndexOf(OVERFLOW_CHECK)) {
                    out.add(decl + " does not end with its one overflow check");
                } else {
                    body = body.substring(0, body.length() - OVERFLOW_CHECK.length());
                }
            }
            for (String w : new String[] { "if", "while", "do", "continue", "break", "switch", "cmp32" }) {
                if (java.util.regex.Pattern.compile("\\b" + w + "\\b").matcher(body).find()) out.add(decl + " has `" + w + "`");
            }
            for (String w : new String[] { "?", "== 0", "!= 0", "&&", "||" }) {
                if (body.contains(w)) out.add(decl + " has `" + w + "`");
            }
            int ret = body.indexOf("return");
            if (ret >= 0 && (ret != body.lastIndexOf("return") || !body.substring(ret).matches("return [^;]*;"))) out.add(decl + " returns before its end");
            java.util.regex.Matcher loop = java.util.regex.Pattern.compile("for \\(short (\\w+) = ([^;]+); \\1 (?:>=|<) ([^;]+);").matcher(body);
            int loops = 0;
            while (loop.find()) {
                loops++;
                String ends = (loop.group(2) + " " + loop.group(3)).replace("(short)", "").replace("len", "LEN");
                if (decl.equals("private static void mulByDelta(")) ends = ends.replace("from", "FROM");
                if (!ends.matches("[0-9A-Z_ ()+-]+")) out.add(decl + ": a loop from " + loop.group(2) + " to " + loop.group(3) + " does not count a constant");
            }
            if (loops != body.split("\\bfor \\(", -1).length - 1) out.add(decl + ": a loop the scan could not read");
        }
        String flat = src.replaceAll("\\s+", " ");
        if (flat.split("cmp32\\(", -1).length - 1 != 1) out.add("an early-exit compare (cmp32) is called in the signer");
        for (String call : new String[] { "mulByDelta(p, pOff, work, tOff, (short)0);", "mulByDelta(work, tOff, work, t2Off, DELTA_FROM);", "mulByDelta(a, aOff, work, workOff, (short)31);" }) {
            if (!flat.contains(call)) out.add("mulByDelta is not called with a constant first row: " + call);
        }
        if (flat.split("mulByDelta\\(", -1).length - 2 != 3) out.add("mulByDelta is called other than three times");
        long named = java.util.regex.Pattern.compile("\\b(?:subIf|under)\\(").matcher(flat).results().count();
        long fixed = java.util.regex.Pattern.compile("\\b(?:subIf|under)\\((?:[^;()]|\\((?:[^()]|\\([^()]*\\))*\\))*,\\s*(?:\\(short\\)\\s*32|len)\\s*\\)").matcher(flat).results().count();
        if (named - 2 != fixed) out.add("a call of subIf or under gives a length other than 32 or its caller's");
        if (flat.indexOf("private short signOnce(") < 0) {
            out.add("no signOnce in the source");
        } else {
            int[] sign = bodyRange(src, "private short signOnce(");
            String body = src.substring(sign[0], sign[1]).replaceAll("\\s+", " ");
            if (!body.contains("subtractFromN(sc, SC_K, sc, SC_E); choose(sc, SC_K, sc, SC_E, (short)(sc[(short)(SC_TMP + 64)] & 1));")) out.add("sign does not choose k or n - k by a mask");
            if (body.contains("ryOdd")) out.add("sign branches on R's parity");
        }
        return out;
    }

    @Test
    @DisplayName("The arithmetic that combines the key and the nonce has no branch and no loop that ends on a value: no if, while, continue, break, return before the end, ?:, compare to zero or early-exit compare in addModN, reduceModN, subIf, under, choose, sub32, subtractFromN, mul256x256, mulByDelta, addDelta, add4x32, reduce512toModN and mulSmallByDelta (its one overflow check at its end aside), every for loop counts a constant, and sign() chooses k or n - k by a mask")
    void theCombiningArithmeticHasNoBranchOnAValue() throws Exception {
        java.util.List<String> v = branchyReductions(new String(java.nio.file.Files.readAllBytes(mainSourceDir().resolve("SchnorrHW.java")), java.nio.charset.StandardCharsets.UTF_8));
        org.junit.jupiter.api.Assertions.assertTrue(v.isEmpty(), String.join("\n", v));
    }

    @Test
    @DisplayName("The reductions' scan can say no: each of the old branches put back into today's source (the carry fold's while, the final compare-and-subtract loop, addModN's if on its carry, reduceModN's if on cmp32, sub's if on the borrow, an early return in under, a second check in mulSmallByDelta, the branch on R's parity, a row count that follows a value) is found")
    void theReductionScanCanSayNo() throws Exception {
        String src = new String(java.nio.file.Files.readAllBytes(mainSourceDir().resolve("SchnorrHW.java")), java.nio.charset.StandardCharsets.UTF_8);
        org.junit.jupiter.api.Assertions.assertTrue(branchyReductions(src).isEmpty(), "today's source passes");
        String[][] plants = {
            { "addDelta(out, outOff, addDelta(out, outOff, carry));", "while (carry != 0) { carry = addDelta(out, outOff, carry); }" },
            { "        reduceModN(out, outOff);\n    }\n\n    /**\n     * v += m", "        while (under(out, outOff, Secp256k1.SECP256K1_N, (short)32) == 0) { subIf(out, outOff, Secp256k1.SECP256K1_N, (short)1, (short)32); }\n    }\n\n    /**\n     * v += m" },
            { "subIf(out, outOff, Secp256k1.SECP256K1_N, (short)(carry | (borrow ^ 1)), (short)32);", "if (carry != 0) subIf(out, outOff, Secp256k1.SECP256K1_N, (short)1, (short)32);" },
            { "short atLeastN = (short)(under(a, aOff, Secp256k1.SECP256K1_N, (short)32) ^ 1);", "short atLeastN = (short)(cmp32(a, aOff, Secp256k1.SECP256K1_N, (short)0) >= 0 ? 1 : 0);" },
            { "            borrow = (short)((diff >> 8) & 1);", "            if (diff < 0) { borrow = 1; } else { borrow = 0; }" },
            { "            borrow = (short)((d >> 8) & 1);\n        }\n        return borrow;", "            borrow = (short)((d >> 8) & 1);\n            if (d < 0) return (short)1;\n        }\n        return borrow;" },
            { "            over = (short)(over | (a[(short)(aOff + i)] & 0xFF));", "            if (a[(short)(aOff + i)] != 0) ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);" },
            { "choose(sc, SC_K, sc, SC_E, (short)(sc[(short)(SC_TMP + 64)] & 1));", "boolean ryOdd = (sc[(short)(SC_TMP + 64)] & 1) == 1; if (ryOdd) Util.arrayCopy(sc, SC_E, sc, SC_K, (short)32);" },
            { "for (short i = 31; i >= from; i--) {", "for (short i = 31; i >= from && a[(short)(aOff+i)] != 0; i--) {" },
        };
        for (String[] p : plants) {
            org.junit.jupiter.api.Assertions.assertEquals(1, src.split(java.util.regex.Pattern.quote(p[0]), -1).length - 1, "the plant's place is in the source once: " + p[0]);
            String planted = src.replace(p[0], p[1]);
            org.junit.jupiter.api.Assertions.assertFalse(branchyReductions(planted).isEmpty(), "the scan passed: " + p[1]);
        }
    }

    @Test
    @DisplayName("mul256x256 (raw 512-bit product) matches BigInteger a*b")
    void mul256x256MatchesBigInteger() throws Exception {
        java.lang.reflect.Method m = SchnorrHW.class.getDeclaredMethod(
            "mul256x256", byte[].class, short.class, byte[].class, short.class,
            byte[].class, short.class);
        m.setAccessible(true);
        Random r = rng();
        for (int i = 0; i < 200; i++) {
            BigInteger a = new BigInteger(256, r).mod(N);
            BigInteger b = new BigInteger(256, r).mod(N);
            byte[] prod = new byte[64];
            m.invoke(null, to32(a), (short) 0, to32(b), (short) 0, prod, (short) 0);
            assertEquals(a.multiply(b), new BigInteger(1, prod),
                "mul256x256 mismatch at iteration " + i);
        }
    }
}
