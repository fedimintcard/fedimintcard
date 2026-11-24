package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.*;
import javacard.security.*;
import javacardx.crypto.Cipher;

/**
 * SchnorrHW — Hardware BIP-340 Schnorr signing for JavaCard 3.0.5+
 *
 * This class replaces the BigInteger-based simulation in CashuApplet with a
 * JavaCard-native implementation. It uses only APIs from:
 *   javacard.framework.*
 *   javacard.security.*
 *
 * No java.math, java.security, java.util, or System.arraycopy.
 *
 * --- Signing algorithm (BIP-340 default signing) ---
 *
 *   1. If P.y is odd, d_eff = n − d  (normalise to even-y key)
 *   2. a = 32 fresh bytes from RandomData.ALG_SECURE_RANDOM
 *      t = d_eff XOR SHA256_tagged("BIP0340/aux", a)
 *   3. k = SHA256_tagged("BIP0340/nonce", t ‖ P.x ‖ msg) mod n
 *   4. R = k·G  (via KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY)
 *   5. If R.y is odd, k = n − k
 *   6. e = SHA256_tagged("BIP0340/challenge", R.x ‖ P.x ‖ msg) mod n
 *   7. s = (k + e·d_eff) mod n, into scratch: e·d by the chip's RSA in two
 *      shares where SET_CARD's test of it held (`chipMul`, D216, D217), by
 *      mulModN where not; then addModN
 *   8. Verify: R' = s·G − e·Q, Q the even-Y point whose x went into e;
 *      R'.x must be R.x and R'.y even, or the card answers 6F00 and gives
 *      nothing (`verify`). On the chip's path a failed check gives the chip
 *      path up for this signature only (D228) and the whole signature is made
 *      again by the bytecode, with a fresh nonce, and checked again; only that
 *      failing is 6F00 (D218, which amends D207)
 *   9. sig = R.x ‖ s (64 bytes), written to the caller's buffer only now
 *
 *   Step 8 is BIP-340's own last step of default signing ("if Verify fails,
 *   abort"). d is in RAM and e·d a multiplication in software, and AUTH
 *   signs for any reader with no PIN, as often as it likes: one fault in that
 *   multiplication, or one bit of d flipped in the scratch, makes a signature
 *   whose error s'·G − R − e·P = e·δ·G leaves few values for δ, a bit or a
 *   byte of d; enough of them give d, and every piece on the card with it.
 *   A signature that does not verify never leaves the card. The waits
 *   (CashuApplet.work) are sign() too, check and all, and their signature
 *   is thrown away by the caller: a wait is a checked signature's length,
 *   the length the holds were measured at (D218, the keeper's answer K1 of
 *   2026-11-18; D207's "the waits keep their length" is amended by it).
 *
 *   Steps 2–3 are BIP-340's *default signing* nonce derivation, auxiliary
 *   randomness and all — not a deterministic RFC6979-style nonce. This matters
 *   on a bearer card: a nonce that is a pure function of (d, msg) hands a fault
 *   injector two signatures over the same k, and d = (s1−s2)/(e1−e2) then falls
 *   straight out. Folding fresh aux randomness into t is precisely the
 *   mitigation BIP-340 specifies, so signatures over the same message are
 *   deliberately NOT reproducible. See SchnorrHWSignTest#signUsesAFreshNonce.
 *
 * --- Modular arithmetic ---
 *
 *   All 256-bit arithmetic is over Z_n where n is the secp256k1 group order.
 *
 *   mulModN uses schoolbook 256×256 → 512-bit product, then reduces via the
 *   identity  2^256 ≡ DELTA (mod n)  where DELTA = 2^256 − n (≈ 2^128).
 *   The reduction applies the identity twice (depth-2) to bring the result
 *   to < 2^256 < 2n, followed by a single conditional subtract.
 *
 * --- The same work whatever the secret ---
 *
 *   Every step that combines the key and the nonce runs the same
 *   instructions, the same number of times, whatever their values: the
 *   multiplies have no skip for a zero byte (D21), and the rows the
 *   reduction leaves out are left out by fixed bounds, the rows that are
 *   zero for every value; the reduction folds its carries back in two fixed
 *   passes and takes n off once, by a mask; the sum k + e·d, the reduction
 *   of a hash and the choice of k or n − k are made by a mask over both
 *   answers (`subIf`, `choose`). No branch and no loop count here depends on
 *   d or k.
 *
 *   What is still not the same work: the virtual machine's own instructions
 *   (a multiply or an array read may take a time, or draw a power, that
 *   follows its operands); the chip's SHA-256 and its key agreement
 *   (R = k·G); the parity of the card's own public key (`pyOdd`: public);
 *   and the length getS answers (a property of the card's key, the same at
 *   every signature).
 *
 * --- Memory discipline ---
 *
 *   JavaCard Classic allocates `new` in persistent EEPROM/Flash and never
 *   collects it: an allocation on the signing path is a permanent leak that
 *   bricks the card after a few hundred taps. Every buffer this class uses is
 *   therefore allocated ONCE — in the constructor / init(), at install time —
 *   as CLEAR_ON_DESELECT transient memory, and threaded down through the
 *   arithmetic helpers as explicit (work, workOff) parameters.
 *
 *   No `new` may appear anywhere outside the constructor and init().
 *
 *   Transient footprint: `sc` (256 B scratchpad) + `work` (288 B arithmetic
 *   scratch, see WORK_LEN), and the temporary key's own room, where the chip
 *   gives it (`keyInRam`).
 *
 *   A secret scalar is in a key object only for the call that uses it: the
 *   nonce in sign(), the scalar addTo() and mul() are handed, and the key
 *   agree() is handed. Each of them ends, however it ends, by `rest()`;
 *   sign() first zeroes `sc` and `work`, which held d, k and e·d. The check
 *   of step 8 begins by zeroing d, k and e·d and taking the nonce out of the
 *   key (`rest()`): it needs none of them, and its own calls put only public
 *   scalars in the key (e, s, a date's t).
 *
 * ENG-182 — cashucard/javacard
 */
final class SchnorrHW {

    // ── secp256k1 group order n ────────────────────────────────────────────
    //
    // Every modular helper in this class reduces against Secp256k1.SECP256K1_N,
    // the one copy of n the package holds; the key objects get the same array
    // through Secp256k1.curve().

    /**
     * DELTA = 2^256 mod n = 2^256 − n
     *
     * n  = FFFF...FFFE BAAEDCE6 AF48A03B BFD25E8C D0364141
     * 2^256 = 1 0000...0000
     * DELTA = 0000...0001 45512319 50B75FC4 402DA173 2FC9BEBF
     *
     * Derived from N, and therefore not independently editable: every reduction
     * below is only correct while N + DELTA == 2^256 exactly. init() asserts
     * that identity at install time.
     */
    private static final byte[] DELTA = {
        (byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,
        (byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x01,
        (byte)0x45,(byte)0x51,(byte)0x23,(byte)0x19,(byte)0x50,(byte)0xB7,(byte)0x5F,(byte)0xC4,
        (byte)0x40,(byte)0x2D,(byte)0xA1,(byte)0x73,(byte)0x2F,(byte)0xC9,(byte)0xBE,(byte)0xBF
    };

    // ── Precomputed tag hashes (set once at install time) ─────────────────
    // SHA256("BIP0340/aux"), SHA256("BIP0340/nonce") and SHA256("BIP0340/challenge")
    // Stored in EEPROM, initialised by init().
    private final byte[] tagHashAux;       // 32 bytes
    private final byte[] tagHashNonce;     // 32 bytes
    private final byte[] tagHashChallenge; // 32 bytes

    // ── Transient scratchpad (CLEAR_ON_DESELECT, 256 bytes) ────────────────
    // Layout (non-overlapping during signing):
    //   [  0.. 31]  d_eff     — private key scalar (normalised)
    //   [ 32.. 63]  Px        — public key x-coordinate
    //   [ 64.. 95]  k         — nonce scalar
    //   [ 96..191]  tmp96     — hash input / intermediate (96 bytes)
    //   [192..223]  e         — challenge scalar
    //   [224..255]  ed        — e * d_eff mod n
    private final byte[] sc;
    private static final short SC_D    = (short)  0;
    private static final short SC_PX   = (short) 32;
    private static final short SC_K    = (short) 64;
    private static final short SC_TMP  = (short) 96;
    private static final short SC_E    = (short)192;
    private static final short SC_ED   = (short)224;

    // ── Transient arithmetic scratch (CLEAR_ON_DESELECT, WORK_LEN bytes) ──
    // Threaded into mulModN as (work, workOff) so that nothing on the signing
    // path ever calls `new`. Layout, relative to workOff:
    //   [  0.. 63]  prod       — 512-bit a*b product           (mulModN)
    //   [ 64..127]  t          — 512-bit p_hi*DELTA            (reduce512toModN)
    //   [128..191]  t2         — 512-bit t_hi*DELTA            (reduce512toModN)
    //   [192..223]  hiContrib  — 256-bit t2_hi*DELTA           (reduce512toModN)
    //   [224..287]  full       — 512-bit scratch               (mulSmallByDelta)
    /** Bytes of scratch mulModN needs; callers must supply at least this much. */
    static final short WORK_LEN      = (short)288;
    /** Offsets inside the work buffer handed to reduce512toModN. */
    private static final short W_PROD = (short)  0;
    private static final short W_RED  = (short) 64;   // reduce512toModN's region
    private static final short W_T    = (short)  0;   // relative to W_RED
    private static final short W_T2   = (short) 64;
    private static final short W_HI   = (short)128;
    private static final short W_SUB  = (short)160;   // mulSmallByDelta's region
    private final byte[] work;

    // ── The check of step 8 (`verify`), and the signature before it leaves ──
    // Once mulModN is done with `work`, s and what the check reads are kept
    // there, and the points it makes go in `sc`, which it zeroes first (d, k
    // and e·d are no part of the check). No engine call is given the same
    // bytes as a point it reads and the point it writes: a chip is not known
    // to take its answer over its input. In `work`:
    //   [  0.. 31]  s   : k + e·d mod n: the half that gives d away where it is wrong
    //   [ 32.. 63]  e   : the challenge
    //   [ 64.. 95]  Rx  : R's x, the other half
    //   [ 96..127]  Px  : the x that went into e
    //   [128..192]  R'  : [s]G + (−T), 65 bytes, which must be R
    // In `sc`:
    //   [  0.. 64]  W   : the card's own point, 04 || X || Y: getW's, or [d]G where getW gives 33 bytes
    //   [ 65..129]  Q   : W + [t]G, the date's key (D197); with no date, Q is W in place
    //   [130..194]  T   : [e]Q, then −T
    private static final short V_S    = (short)  0;
    private static final short V_E    = (short) 32;
    private static final short V_RX   = (short) 64;
    private static final short V_PX   = (short) 96;
    private static final short V_R    = (short)128;
    private static final short V_W    = (short)  0;
    private static final short V_Q    = (short) 65;
    private static final short V_T    = (short)130;

    // ── Crypto objects (allocated once) ───────────────────────────────────
    private final MessageDigest sha256;
    private final KeyAgreement  ecdh;      // ALG_EC_SVDP_DH_PLAIN_XY
    private final KeyAgreement  gm;        // ALG_EC_PACE_GM: [s]G + H, the chip's point addition, for a change output's blinding (1.12)
    private final ECPrivateKey  tmpPriv;   // the temporary key: k in sign(), the scalar of addTo() and mul()
    private final boolean       keyInRam;  // the chip gave tmpPriv as a key cleared on deselect
    private final RandomData    rng;       // BIP-340 auxiliary randomness (step 2)
    // NB: there is deliberately no ECPublicKey for the generator. R = k*G is
    // computed by handing the raw 65-byte G to generateSecret(), so a key object
    // for G would be built, parameterised and then never read — dead weight in
    // EEPROM and one more thing to keep in sync with the curve parameters.

    // ── Constructor ───────────────────────────────────────────────────────

    SchnorrHW() {
        tagHashAux       = new byte[32];
        tagHashNonce     = new byte[32];
        tagHashChallenge = new byte[32];

        sc     = JCSystem.makeTransientByteArray((short)256, JCSystem.CLEAR_ON_DESELECT);
        work   = JCSystem.makeTransientByteArray(WORK_LEN,   JCSystem.CLEAR_ON_DESELECT);
        sha256 = MessageDigest.getInstance(MessageDigest.ALG_SHA_256, false);

        ecdh = KeyAgreement.getInstance(KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY, false);
        gm   = KeyAgreement.getInstance(KeyAgreement.ALG_EC_PACE_GM, false);

        // BIP-340 auxiliary randomness. Allocated once, like everything else on
        // the signing path.
        //
        // ALG_SECURE_RANDOM (and generateData) are marked deprecated in the
        // 3.0.5 API in favour of ALG_TRNG / ALG_PRESEEDED_DRBG / nextBytes, and
        // the CAP build prints a warning for each. That is deliberate: the
        // replacements are 3.0.5-era additions that not every 3.0.5+ part
        // actually implements, and getInstance on an unsupported algorithm
        // throws CryptoException at install. ALG_SECURE_RANDOM is present
        // everywhere this applet can run. Revisit only with a card in hand.
        rng = RandomData.getInstance(RandomData.ALG_SECURE_RANDOM);

        // The temporary key, built last, after every other object here, so that
        // a chip short of room for it says so at this buildKey (or at its first
        // use, the setS below), inside the fallback, and not at an object made
        // after it: in RAM, cleared on deselect and at power loss, where the
        // chip has such a key and the room for it; where it throws on the type,
        // on the room or on its use, the persistent kind, and `keyInRam` says
        // which, so that rest() knows how to wipe it. A key in RAM loses its
        // curve with its scalar, so ready() sets the curve again before each
        // use; the persistent one keeps it. It starts out holding G's x, a
        // public number.
        ECPrivateKey k = null;
        byte type = KeyBuilder.TYPE_EC_FP_PRIVATE_TRANSIENT_DESELECT;
        while (k == null) {
            try {
                k = (ECPrivateKey) KeyBuilder.buildKey(type, KeyBuilder.LENGTH_EC_FP_256, false);
                Secp256k1.curve(k);
                k.setS(Secp256k1.SECP256K1_G, (short) 1, (short) 32);
            } catch (CardRuntimeException e) {
                if (type == KeyBuilder.TYPE_EC_FP_PRIVATE) throw e;
                type = KeyBuilder.TYPE_EC_FP_PRIVATE;
                k = null;
            }
        }
        tmpPriv  = k;
        keyInRam = type != KeyBuilder.TYPE_EC_FP_PRIVATE;
    }

    /**
     * Must be called once during applet install (after constructor) to
     * precompute and store the BIP-340 tag hashes in EEPROM, and to verify
     * DELTA against n. The chip engines' framing is checked at every SET_CARD
     * (checkFraming), not here.
     */
    void init() {
        // DELTA is defined as 2^256 − N, i.e. N + DELTA must be exactly 2^256:
        // a 256-bit sum of zero with a carry out. mulModN's whole reduction
        // rests on that identity, so it is checked rather than commented.
        // Uses sc as scratch: allocated already, and nothing has signed yet.
        if (add32(Secp256k1.SECP256K1_N, (short)0, DELTA, (short)0, sc, (short)0) != (short)1) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        // and DELTA's first bytes are zero, which the product by it (`mulByDelta`) leaves out
        for (short i = 0; i < DELTA_FROM; i++) {
            if (DELTA[i] != (byte)0) ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        for (short i = 0; i < 32; i++) {
            if (sc[i] != (byte)0) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
        }

        // The three tags, hashed once. Written out here rather than kept as
        // constants of the class: each constant is one more entry in the
        // package's constant pool, and the chip takes only so many at an
        // install (docs/CASHU-CARD-HARDWARE.md). A fresh digest is in its
        // initial state, and doFinal leaves it there for the next.
        byte[] tagAux = { 'B','I','P','0','3','4','0','/','a','u','x' };
        sha256.doFinal(tagAux, (short)0, (short)11, tagHashAux, (short)0);
        byte[] tagNonce = { 'B','I','P','0','3','4','0','/','n','o','n','c','e' };
        sha256.doFinal(tagNonce, (short)0, (short)13, tagHashNonce, (short)0);
        byte[] tagChallenge = {
            'B','I','P','0','3','4','0','/','c','h','a','l','l','e','n','g','e'
        };
        sha256.doFinal(tagChallenge, (short)0, (short)17, tagHashChallenge, (short)0);
    }

    // ── Public API ────────────────────────────────────────────────────────

    /**
     * BIP-340 Schnorr sign using JavaCard hardware APIs.
     *
     * @param privKey  card's secp256k1 EC private key
     * @param pubKey   card's secp256k1 EC public key (for P.y parity check)
     * @param msg      source buffer containing exactly 32-byte message
     * @param msgOff   offset of message in source buffer
     * @param out      output buffer (must have ≥ 64 bytes from outOff)
     * @param outOff   offset in output buffer
     * @return 64 (signature byte length)
     */
    short sign(ECPrivateKey privKey, ECPublicKey pubKey,
               byte[] msg, short msgOff,
               byte[] out, short outOff) {
        return sign(privKey, pubKey, null, (short)0, msg, msgOff, out, outOff);
    }

    /**
     * BIP-340 under the card's key for a date (keys per load, D197): with `g` not null, the private key is d + t mod n, t the
     * 32 bytes at g[gOff], and its public key the compressed point after them (33), P + t*G, which the caller worked out; with
     * `g` null, the key itself. `g` is read before anything is written to `out`.
     *
     * The signature is verified before it is written (step 8, `verify`): `6F00` (SW_CRYPTO_ERROR) for one that does not
     * verify under the key whose x went into its challenge, for a public key object that names another key than the private
     * one, and for a point at g + 32 that is not P + t*G with its parity, with nothing written to `out` and nothing of s left
     * in this class's scratch. Every signature that leaves the card is made here.
     */
    short sign(ECPrivateKey privKey, ECPublicKey pubKey, byte[] g, short gOff,
               byte[] msg, short msgOff,
               byte[] out, short outOff) {
        // d, k and e·d are in sc and work, and k in the temporary key, for this
        // call and no longer: however it ends, the two are zeroed first and
        // then the key is wiped, so a wipe the chip refuses leaves neither
        try {
            if (chipMul) {
                short made = signOnce(privKey, pubKey, g, gOff, msg, msgOff, out, outOff, true);
                if (made != (short)0) return made;
                // The chip's product did not give a signature that verifies (or the chip refused the work): the chip path
                // is given up for this signature only (D228: chipMul stays as SET_CARD's test left it, so one induced fault
                // does not turn the chip's path off for every later signature; its one writer is chipMulInit, run at the
                // first SET_CARD of a set-up life, after the install or a RESET_CARD, D233), and the whole signature is made
                // again by the bytecode, from step 1 with a fresh nonce: the check wiped d, and the failed signature never
                // left the card (D218).
                // [M2: the plan's recommended join, not yet agreed by the other maintainer; to flip to his "6F00 at once",
                // throw SW_CRYPTO_ERROR here instead of signing again.]
                Util.arrayFillNonAtomic(sc, (short)0, (short)256, (byte)0);
                Util.arrayFillNonAtomic(work, (short)0, WORK_LEN, (byte)0);
                rest();
            }
            return signOnce(privKey, pubKey, g, gOff, msg, msgOff, out, outOff, false);
        } finally {
            Util.arrayFillNonAtomic(sc, (short)0, (short)256, (byte)0);
            Util.arrayFillNonAtomic(work, (short)0, WORK_LEN, (byte)0);
            rest();
        }
    }

    /**
     * Steps 1 to 9 once: with `chip`, e·d by the chip in two shares (`splitProduct`), and 0 (nothing written to `out`) where
     * the chip refused that work or the signature it gave does not verify, for sign() to make it again by the bytecode; with
     * `chip` false, e·d by `mulModN`, and 6F00 where the signature does not verify. 64 once `out` holds the signature.
     */
    private short signOnce(ECPrivateKey privKey, ECPublicKey pubKey, byte[] g, short gOff,
                           byte[] msg, short msgOff,
                           byte[] out, short outOff, boolean chip) {

        // ── Step 1: Extract d, check P.y parity ──────────────────────
        // getS is specified to return the scalar's byte length, with the value
        // right-aligned in the caller's buffer. It is NOT specified to left-pad
        // to the key size, so a card that strips leading zero bytes is entirely
        // spec-legal — and roughly one generated key in 256 has a zero high
        // byte. Two things therefore must not happen here:
        //
        //   * trusting the length blindly. Everything below reads d as exactly
        //     32 bytes at sc[SC_D..SC_D+31]; a short answer would fold the tail
        //     of the shared scratchpad into d, and every signature the card
        //     emitted would fail verification while its proofs stayed
        //     P2PK-locked to that key — unspendable.
        //   * refusing the card. The key is generated on-card at install time
        //     and can have a zero high byte by pure luck, so rejecting a short
        //     length would brick a card's worth of bearer money at random.
        //
        // Right-aligning handles both, and is correct on every spec-legal card.
        short dLen = privKey.getS(sc, SC_D);   // d at sc[SC_D..SC_D+dLen-1]
        if (dLen < (short)1 || dLen > (short)32) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        if (dLen < (short)32) {
            short pad = (short)(32 - dLen);
            // Backwards, byte by byte: source and destination overlap inside
            // sc, and the JavaCard spec does not promise arrayCopyNonAtomic
            // behaves like memmove when they do.
            for (short i = (short)(dLen - 1); i >= (short)0; i--) {
                sc[(short)(SC_D + pad + i)] = sc[(short)(SC_D + i)];
            }
            // Not a fresh array: sc is long-lived scratch that still holds the
            // previous signature's bytes, so the pad must be written, not
            // assumed.
            Util.arrayFillNonAtomic(sc, SC_D, pad, (byte)0);
        }

        // Get public key point W (uncompressed 65 bytes or compressed 33 bytes): the date's, P + t*G, with d + t for its key
        short wLen = (short)33;
        if (g != null) {
            addModN(sc, SC_D, g, gOff, sc, SC_D);
            Util.arrayCopyNonAtomic(g, (short)(gOff + 32), sc, SC_TMP, wLen);
        } else {
            wLen = pubKey.getW(sc, SC_TMP);  // W at sc[96..]
        }
        boolean pyOdd;
        if (wLen == (short)65) {
            // Uncompressed: 04 || Px(32) || Py(32)
            // A 65-byte W whose lead byte is not 0x04 is not a point encoding we
            // understand. Falling through to the compressed branch here would
            // read the "parity" out of a byte that is not a parity byte and emit
            // a signature no mint can verify, so refuse instead.
            if (sc[SC_TMP] != (byte)0x04) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
            // P.y is sc[TMP+33..TMP+64]; parity = LSB of last byte
            pyOdd = (sc[(short)(SC_TMP + 64)] & 1) == 1;
            Util.arrayCopy(sc, (short)(SC_TMP + 1), sc, SC_PX, (short)32);
        } else if (wLen == (short)33
                   && (sc[SC_TMP] == (byte)0x02 || sc[SC_TMP] == (byte)0x03)) {
            // Compressed: prefix 02 (even) or 03 (odd)
            pyOdd = (sc[SC_TMP] & 1) == 1;
            Util.arrayCopy(sc, (short)(SC_TMP + 1), sc, SC_PX, (short)32);
        } else {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            return (short)0;   // unreachable; keeps pyOdd definitely assigned
        }

        if (pyOdd) {
            subtractFromN(sc, SC_D, sc, SC_D);   // d = n − d
        }

        // ── Step 2: t = d_eff XOR taggedHash("BIP0340/aux", a) ───────
        // BIP-340 default signing. `a` is fresh secure randomness per signature;
        // it is what stops a fault-injected repeat of this call from reusing k.
        // sc[TMP..TMP+31] = a, sc[TMP+32..TMP+63] = taggedHash(aux, a) -> t
        rng.generateData(sc, SC_TMP, (short)32);
        taggedHash(tagHashAux, sc, SC_TMP, (short)32, sc, (short)(SC_TMP+32));
        for (short i = 0; i < 32; i++) {
            sc[(short)(SC_TMP+32+i)] =
                (byte)(sc[(short)(SC_TMP+32+i)] ^ sc[(short)(SC_D+i)]);
        }

        // ── Step 3: k = taggedHash("BIP0340/nonce", t ‖ P.x ‖ msg) mod n
        // sc[TMP..TMP+95] = t(32) || Px(32) || msg(32)
        Util.arrayCopy(sc,  (short)(SC_TMP+32), sc, SC_TMP,          (short)32); // t
        Util.arrayCopy(sc,  SC_PX,              sc, (short)(SC_TMP+32),(short)32); // Px
        Util.arrayCopy(msg, msgOff,             sc, (short)(SC_TMP+64),(short)32); // msg
        taggedHash(tagHashNonce, sc, SC_TMP, (short)96, sc, SC_K);
        reduceModN(sc, SC_K);         // k = k mod n (in place)

        // ── Step 4: R = k·G via ECDH coprocessor ─────────────────────
        // Load k into temp private key, compute [k]G
        ready();
        tmpPriv.setS(sc, SC_K, (short)32);
        ecdh.init(tmpPriv);
        // Result = 65-byte uncompressed point at sc[TMP]; we need 65 bytes
        // sc[TMP..TMP+64] = 04 || Rx(32) || Ry(32)
        //
        // ALG_EC_SVDP_DH_PLAIN_XY output framing is NOT uniform across
        // implementations — some cards return the bare 64-byte X||Y. Everything
        // below reads Rx at SC_TMP+1 and the y-parity at SC_TMP+64, so an
        // unexpected framing would silently shift both by one byte and emit
        // well-formed signatures that no mint can verify.
        //
        // A card that frames it otherwise is refused at its first SET_CARD
        // (checkFraming), and is checked again here: it signs nothing.
        framed(ecdh.generateSecret(Secp256k1.SECP256K1_G, (short) 0, (short)65, sc, SC_TMP), sc, SC_TMP);

        // ── Step 5: if R.y is odd, k = n − k ─────────────────────────
        // n − k is made either way, into SC_E (free until step 6), and a mask
        // keeps k or takes it: no branch on R's parity
        subtractFromN(sc, SC_K, sc, SC_E);
        choose(sc, SC_K, sc, SC_E, (short)(sc[(short)(SC_TMP + 64)] & 1));
        // Rx is at sc[TMP+1..TMP+32]

        // ── Step 6: e = taggedHash("BIP0340/challenge", Rx ‖ Px ‖ msg) mod n
        // Re-use sc[TMP..TMP+95] for the 96-byte input
        // sc[TMP..TMP+31]  = Rx  (already there at SC_TMP+1, shift left by 1)
        Util.arrayCopy(sc, (short)(SC_TMP+1), sc, SC_TMP,          (short)32); // Rx
        Util.arrayCopy(sc, SC_PX,             sc, (short)(SC_TMP+32),(short)32); // Px
        Util.arrayCopy(msg, msgOff,           sc, (short)(SC_TMP+64),(short)32); // msg
        taggedHash(tagHashChallenge, sc, SC_TMP, (short)96, sc, SC_E);
        reduceModN(sc, SC_E);         // e = e mod n

        // ── Step 7: s = (k + e·d) mod n ──────────────────────────────
        // e·d by the chip's RSA where SET_CARD's test of it held (D216), d never given to the engine alone but as two
        // shares of a fresh random split (D217); by the bytecode where not. s goes into work (V_S), and not into `out`:
        // nothing of the signature is where the host can read it until step 8 has passed. Rx beside it: step 6 shifted it
        // to sc[SC_TMP..SC_TMP+31] before hashing (the challenge hash wrote to SC_E, not SC_TMP), and the check zeroes sc.
        if (chip) {
            try {
                splitProduct();
            } catch (RuntimeException x) {
                return (short)0;                                 // the chip refused: sign() makes it again by the bytecode
            }
        } else {
            mulModN(sc, SC_E, sc, SC_D, sc, SC_ED, work, (short)0);  // ed = e * d mod n
        }
        addModN(sc, SC_ED, sc, SC_K, work, V_S);                 // s = ed + k mod n
        Util.arrayCopyNonAtomic(sc, SC_TMP, work, V_RX, (short)32);

        // ── Step 8: the signature verified, or nothing out ───────────
        // A key or a date's point that does not agree is 6F00 inside verify(), whichever path made e·d (not the chip's
        // doing: the bytecode would fail it too). A signature that does not verify is the chip's to answer for on its path
        // (0: sign() signs again by the bytecode), and 6F00 on the bytecode's (D207, D218).
        boolean good;
        if (chip) {
            try {
                good = verify(privKey, pubKey, g, gOff);
            } catch (CryptoException x) {
                good = false;
            }
            if (!good) return (short)0;
        } else if (!verify(privKey, pubKey, g, gOff)) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }

        // ── Step 9: output = Rx ‖ s ──────────────────────────────────
        Util.arrayCopyNonAtomic(work, V_RX, out, outOff, (short)32);
        Util.arrayCopyNonAtomic(work, V_S, out, (short)(outOff + 32), (short)32);

        return (short)64;
    }

    // ── The signature's product by the chip, in two shares (D217) ─────

    /**
     * SC_ED = e·d mod n by the chip, d never alone: a fresh random d1 (under n) and d2 = d - d1 mod n, then e·d = e·d1 + e·d2,
     * each product by two squarings (`mulModNChip`), four squarings in all. Every operand the engine sees, e + d1 and |e - d1|,
     * e + d2 and |e - d2|, is uniform and independent of d taken alone, whatever e is (e is public in every signature: a mask
     * by it only, as D216 had it, hid nothing). The shares sit in sc[SC_TMP + 32, SC_TMP + 96) (Rx is below them) and the
     * second product in work[256, 288), cleared here once added; the shares are cleared here too, and sc and work again by the
     * check and by sign()'s `finally` on every way out. d at sc[SC_D] is read and left as it is.
     */
    private void splitProduct() {
        short d1 = (short)(SC_TMP + 32), d2 = (short)(SC_TMP + 64), second = (short)256;
        rng.generateData(sc, d1, (short)32);
        reduceModN(sc, d1);
        short borrow = sub32(sc, SC_D, sc, d1, sc, d2);       // d - d1, and whether it went below zero
        addIf(sc, d2, Secp256k1.SECP256K1_N, borrow);       // + n then: d2 = d - d1 mod n
        mulModNChip(sc, SC_E, sc, d1, sc, SC_ED, work, (short)0);
        Util.arrayFillNonAtomic(work, (short)0, (short)256, (byte)0);
        mulModNChip(sc, SC_E, sc, d2, work, second, work, (short)0);
        Util.arrayFillNonAtomic(work, (short)0, (short)256, (byte)0);
        Util.arrayFillNonAtomic(sc, d1, (short)64, (byte)0);
        addModN(sc, SC_ED, work, second, sc, SC_ED);
        Util.arrayFillNonAtomic(work, second, (short)32, (byte)0);
    }

    /**
     * Step 8, BIP-340's "if Verify(P, m, sig) fails, abort": whether s*G - e*Q is R, with R's x (work[V_RX]) and an even
     * Y, where Q is the key the signature is under, whole and with an even Y (BIP-340's lift_x), and its x the Px that went
     * into e (work[V_PX]). False where it is not (signOnce answers 6F00 for it on the bytecode's path, and sign() signs
     * again on the chip's, D218); `6F00` (SW_CRYPTO_ERROR) at once for a key or a date's point that does not agree, which no
     * signing again would mend. Either way before anything is written to the caller's buffer; sign()'s `finally` then
     * zeroes sc and work, s among them.
     *
     * Q comes of the card's own point W and nothing the signing worked out: W is getW's 65 bytes, or, where the chip's getW
     * gives the 33-byte form, [d]G by the key agreement with the card's private key (never a square root, which would take
     * the parity on trust). With `g` (a date's key, D197) Q = [t]G + W by the point addition (t is public) and must be the
     * caller's 33-byte point at g + 32, its x and its parity: a wrong point there is refused, whatever the signature. Every
     * scalar this puts in the temporary key (t, e, s) is public once the signature is out; [d]G gives the agreement the card's
     * own key object, as agree() does for the PIN key; d, k and e·d are zeroed and the nonce taken out of the key and both
     * agreements first. Its own calls each end by rest().
     *
     * What jCardSim cannot show of it (D10): the chip's answers for a scalar of 0 or a sum at infinity, which no key, date
     * or message reaches but by chance (e = 0, s = 0, t = d or t = -d, each about 2^-256); and its time on a chip.
     */
    private boolean verify(ECPrivateKey privKey, ECPublicKey pubKey, byte[] g, short gOff) {
        // what the check reads, beside s and Rx; then d, k and e·d (all of sc) zeroed, and the nonce out of the key
        Util.arrayCopyNonAtomic(sc, SC_E, work, V_E, (short)32);
        Util.arrayCopyNonAtomic(sc, SC_PX, work, V_PX, (short)32);
        Util.arrayFillNonAtomic(sc, (short)0, (short)256, (byte)0);
        rest();

        // W, the card's own point, whole
        short wLen = pubKey.getW(sc, V_W);
        if (wLen == (short)33) {
            agree(privKey, Secp256k1.SECP256K1_G, (short)0, (short)65, sc, V_W);
        } else if (wLen != (short)65 || sc[V_W] != (byte)0x04) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        // Q: W, or W + [t]G, which must be the point the caller named, x and parity
        short q = V_W;
        if (g != null) {
            addToG(g, gOff, sc, V_W, sc, V_Q);
            q = V_Q;
            if (g[(short)(gOff + 32)] != (byte)(0x02 | (sc[(short)(V_Q + 64)] & 1))
                || Util.arrayCompare(sc, (short)(V_Q + 1), g, (short)(gOff + 33), (short)32) != (byte)0) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
        }
        // its x the one in e; and its Y made even (p - Y), as lift_x has it
        if (Util.arrayCompare(sc, (short)(q + 1), work, V_PX, (short)32) != (byte)0) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        if ((sc[(short)(q + 64)] & 1) != 0) sub32(Secp256k1.SECP256K1_P, (short)0, sc, (short)(q + 33), sc, (short)(q + 33));

        // T = [e]Q by the key agreement, made -T; R' = [s]G + (-T) by the point addition
        mul(work, V_E, sc, q, sc, V_T);
        sub32(Secp256k1.SECP256K1_P, (short)0, sc, (short)(V_T + 33), sc, (short)(V_T + 33));
        addToG(work, V_S, sc, V_T, work, V_R);
        return (work[(short)(V_R + 64)] & 1) == 0
            && Util.arrayCompare(work, (short)(V_R + 1), work, V_RX, (short)32) == (byte)0;
    }

    // ── Tagged hash ───────────────────────────────────────────────────────

    /**
     * BIP-340 tagged hash:  SHA256(tagHash ‖ tagHash ‖ msg)
     *
     * @param tagHash  precomputed SHA256(tag),  32 bytes
     * @param msg      message buffer
     * @param msgOff   offset in msg
     * @param msgLen   length of message
     * @param out      output buffer (receives 32-byte hash)
     * @param outOff   offset in output buffer
     */
    private void taggedHash(byte[] tagHash,
                            byte[] msg, short msgOff, short msgLen,
                            byte[] out, short outOff) {
        sha256.reset();
        sha256.update(tagHash, (short)0, (short)32);
        sha256.update(tagHash, (short)0, (short)32);
        sha256.doFinal(msg, msgOff, msgLen, out, outOff);
    }

    // ── Modular arithmetic ────────────────────────────────────────────────

    /**
     * The point a private key and a public point share (04 || X || Y), by this
     * class's own key agreement. For the applet's PIN key: one key agreement
     * object on the card, and the one whose framing SET_CARD checks
     * (checkFraming). The agreement is given G's x again on every way out
     * (rest), so it keeps nothing of the PIN key after the call.
     */
    short agree(ECPrivateKey priv, byte[] pub, short pubOff, short pubLen, byte[] out, short outOff) {
        // the sender's point is 65 bytes (its caller has held it to the curve, CashuApplet.requireFullPoint); and the
        // answer framed as the chip's key agreement must frame it
        if (pubLen != (short)65) ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        try {
            ecdh.init(priv);
            framed(ecdh.generateSecret(pub, pubOff, pubLen, out, outOff), out, outOff);
        } finally {
            rest();
        }
        return (short)65;
    }

    /**
     * Whether the chip's key agreement takes the 65 bytes at p[off] (04 || x || y, its caller has held them to that form,
     * x and y under p: the agreement reduces both) as a point of the curve (D212): one key agreement with the temporary key
     * at its resting scalar, G's x, a public number, so that the answer, [G.x]p, which goes into sc and nowhere else,
     * tells nothing of any key, and a point of another curve or of the twist learns nothing either. The J3R180 refuses
     * every point off the curve at generateSecret (CryptoException); a simulator throws its own exception: any is a
     * refusal. The key is left at that public scalar (no rest()): every other use of it sets the curve and its scalar
     * first. Leaned on only where `checksCurve` held.
     */
    boolean agrees(byte[] p, short off) {
        ready();
        tmpPriv.setS(Secp256k1.SECP256K1_G, (short) 1, (short) 32);
        ecdh.init(tmpPriv);
        try {
            ecdh.generateSecret(p, off, (short) 65, sc, SC_TMP);
        } catch (RuntimeException x) {
            return false;
        }
        return true;
    }

    /**
     * A point of the twist, for the install's test of the agreement: x = 5, whose x^3 + 7 is no square (the point is on no curve
     * of ours), and the y with y^2 = -(x^3 + 7) mod p, which the twist's equation (p is 3 mod 4: -1 is no square) has for it.
     */
    private static final byte[] TWIST_Y = {
        (byte)0x35,(byte)0x0A,(byte)0xE3,(byte)0xB4,(byte)0x80,(byte)0x47,(byte)0xAD,(byte)0xAC,
        (byte)0xDE,(byte)0xEA,(byte)0x49,(byte)0xFB,(byte)0x8A,(byte)0x0B,(byte)0x28,(byte)0x9A,
        (byte)0x94,(byte)0xF7,(byte)0x26,(byte)0x80,(byte)0x10,(byte)0x78,(byte)0x40,(byte)0x8A,
        (byte)0xBA,(byte)0x79,(byte)0x63,(byte)0x1F,(byte)0xA7,(byte)0xA1,(byte)0xB6,(byte)0xBA
    };

    /**
     * The install's test of the agreement (D212, D219): G is taken, and each of six points that are not on the curve is not:
     * G with y one more; an x with no root (5) with G's y; the twist's point (5 and the y of the twist's equation); G's x with
     * G's y and its top bit flipped (a valid x with a wrong y); G's x with y = p - 1 (a y near p); and 04 with zeros. Where any
     * one fails the card never leans on the agreement for a piece's point and refuses the 04 form (6B00): a chip whose agreement
     * skips the curve, checks it in part, or refuses good points, is not leaned on. `work` is the room.
     */
    boolean checksCurve() {
        byte[] g = Secp256k1.SECP256K1_G;
        Util.arrayCopyNonAtomic(g, (short) 0, work, (short) 0, (short) 65);
        if (!agrees(work, (short) 0)) return false;
        for (short k = 0; k < 6; k++) {
            Util.arrayCopyNonAtomic(g, (short) 0, work, (short) 0, (short) 65);
            if (k == 0) {
                work[64]++;
            } else if (k == 1 || k == 2) {
                Util.arrayFillNonAtomic(work, (short) 1, (short) 32, (byte) 0);
                work[32] = (byte) 5;
                if (k == 2) Util.arrayCopyNonAtomic(TWIST_Y, (short) 0, work, (short) 33, (short) 32);
            } else if (k == 3) {
                work[33] ^= (byte) 0x80;
            } else if (k == 4) {
                Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_P, (short) 0, work, (short) 33, (short) 32);
                work[64]--;
            } else {
                Util.arrayFillNonAtomic(work, (short) 1, (short) 64, (byte) 0);
            }
            if (agrees(work, (short) 0)) return false;
        }
        return true;
    }

    /**
     * out = [s]G + H by the chip (TR-03111's generic mapping, which is a
     * scalar multiplication and a point addition in one): 65 bytes,
     * 04 || x || y. `s` is 32 bytes under n; `h` a point on the curve, 65
     * bytes uncompressed. For a change output's blinding (1.12).
     */
    void addToG(byte[] s, short sOff, byte[] h, short hOff, byte[] out, short outOff) {
        addTo(s, sOff, Secp256k1.SECP256K1_G, (short) 0, h, hOff, out, outOff);
    }

    /**
     * out = [s]g + h by the chip: addToG with the key's generator set to the
     * point g (65 bytes, uncompressed) first. The generic mapping multiplies
     * whatever generator its key has, which is what makes it the addition of
     * any two points. Every use of the mapping sets the generator it means,
     * so none is left over from another. For LOAD_CHANGE (3.0, D183): a
     * blinding factor taken off, and a mint's proof checked.
     */
    void addTo(byte[] s, short sOff, byte[] g, short gOff, byte[] h, short hOff, byte[] out, short outOff) {
        // s may be a secret (a change output's blinding factor): it is in the key for this call only, however the call ends
        try {
            ready();
            tmpPriv.setG(g, gOff, (short) 65);
            tmpPriv.setS(s, sOff, (short) 32);
            gm.init(tmpPriv);
            framed(gm.generateSecret(h, hOff, (short) 65, out, outOff), out, outOff);
        } finally {
            rest();
        }
    }

    /**
     * out = [k]p by the chip's key agreement, 65 bytes (04 || x || y): `k` 32
     * bytes under n, `p` a point on the curve, 65 bytes uncompressed. For
     * LOAD_CHANGE (3.0, D183). The agreement multiplies the point it is
     * given, and not the key's generator, so what addTo leaves there does not
     * matter here or to sign().
     */
    void mul(byte[] k, short kOff, byte[] p, short pOff, byte[] out, short outOff) {
        // k may be a secret (the sealing scalar): in the key for this call only, however the call ends
        try {
            ready();
            tmpPriv.setS(k, kOff, (short) 32);
            ecdh.init(tmpPriv);
            framed(ecdh.generateSecret(p, pOff, (short) 65, out, outOff), out, outOff);
        } finally {
            rest();
        }
    }

    /**
     * The framing of both chip engines, which sign(), mul(), agree(), addTo()
     * and addToG() read, and the point each answers. Seventeen asks, each
     * answer 65 bytes, 04 || X || Y, held whole to the point it must be:
     *
     *   the key agreement (mul):   [11]G = 11G;  [n - 1](11G) = -11G;
     *                              [2](-G) = -2G;  [n - 1](2G) = -2G;
     *   the point addition (addTo): [13]G + (-2G) = 11G, the generator G;
     *                              [n - 2](11G) + 11G = -11G, the generator 11G;
     *                              [4](-G) + 2G = -2G, the generator -G;
     *   G with a scalar of the full width: [n - 1]G = -G by the key agreement,
     *                              [n - 2]G + G = -G by the point addition;
     *   k = SHA-256("BIP0340/aux") (tagHashAux, made at install), a scalar of
     *   256 bits whose digits nobody chose: [k](2G) by the key agreement, then
     *   [k - 1](2G) + 2G (the generator 2G) and [2k - 2]G + 2G (the generator
     *   G) by the point addition and [2k]G by the key agreement, each held to
     *   the first: all four are [2k]G;
     *   then the same four with m, BIP-340's tagged hash of nothing under
     *   "BIP0340/nonce" (SHA-256(tagHashNonce || tagHashNonce)), all four
     *   [2m]G. m is the one of twenty fixed hashes tried (the hashes of
     *   BIP-340's nonce and challenge tags, the tagged hash of nothing under
     *   each of its three tags, and SHA-256 applied one to five times to each
     *   tag's hash) with which these scalars, on each path of each engine,
     *   use every entry of a 5-bit fixed window aligned at bit 0 and of a
     *   comb of 5 teeth on the bare scalar (the sweep's forms, below).
     *
     * Among the points given and answered, Y is odd and even, the root
     * (x^3 + 7)^((p+1)/4) and not, and of X's parity and not (-G, 2G); the
     * bases and generators are G and three others; the points added have an
     * odd Y (-2G, 11G) and an even Y, the root (2G), and one is G. A wrong
     * engine is a choice, for each part, of how it reads the base or
     * generator's Y and the added point's Y (its own, p - Y, the even or odd
     * root, the root or p less it, the root with X's parity or not, its own
     * only where it is G), how it takes the scalar (s, or [n - s] above or
     * below n/2), and how it gives the middle point's and the answer's Y (the
     * same rules, or a parity taken from the scalar, its top bit, or a point
     * given): every such engine, one fault or several at once (612 key
     * agreements and 44,064 point additions in all), that answers otherwise
     * at any input answers another point at one of these asks, each engine
     * alone and both (SchnorrHWSignTest's family).
     *
     * The last ten reach the way each engine multiplies, on G (the base or generator of every
     * signature, change blinding and date key) and on another point, with scalars of the full width. A
     * fault of the multiplication itself is refused where one of these scalars, on its path, uses it:
     * a G path that adds n to the scalar and loses the carry, and an entry stored wrong in a table of
     * fixed windows aligned at bit 0, or of a comb or a NAF worked on the bare scalar, of 3 to 5 bits,
     * on either path and either engine: in those forms every entry is used (SchnorrHWSignTest's
     * sweep). A fault these asks do not reach is not refused: an entry none of these scalars uses in
     * another form (a 5-bit window aligned from the top or at bits 1 to 4; a 5-bit window or comb
     * worked on the fixed-length s + n or s + 2n; a table of 6 bits, of which 6 to 16 entries go
     * unused, Booth recoding of 6 or 7 bits, or a NAF of 7 bits; a scalar blinded at random, which
     * fixed asks reach only by chance), a case of the addition these points never meet (a point added
     * to itself), a fault that shows for other scalars only, an engine outside these models, or one
     * built to answer these points right and others wrong. The chip run's real signatures and real
     * change are the last check. Each addition adds two distinct points, as the card adds them.
     *
     * `6F00` (SW_CRYPTO_ERROR) for a chip that answers any of them otherwise.
     * Asked at every SET_CARD, before anything is written (not at install,
     * where the chip takes as little work as it can): a card whose chip
     * answers otherwise is never set up, and so never takes a piece. `out` is
     * 65 bytes of working room; the scalars, all public, are made in
     * sc[0..32), -G at sc[32..97), 2G at sc[97..162) and [2k]G, then
     * [2m]G, at sc[162..227). The temporary key's generator is G again after it,
     * however it ends.
     */
    void checkFraming(byte[] out, short outOff) {
        // sc and G held in locals: one byte a use where a field or a static is three
        byte[] s = sc, g = Secp256k1.SECP256K1_G;
        try {
            Util.arrayFillNonAtomic(s, (short)0, (short)32, (byte)0);
            s[31] = (byte)11;
            mul(s, (short)0, g, (short)0, out, outOff);
            isPoint(out, outOff, ELEVEN_G, (short)0, false);
            s[31] = (byte)13;
            addToG(s, (short)0, MINUS_TWO_G, (short)0, out, outOff);
            isPoint(out, outOff, ELEVEN_G, (short)0, false);
            // n - 1, then n - 2: n's last byte is 41, so neither borrows
            Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_N, (short)0, s, (short)0, (short)32);
            s[31]--;
            mul(s, (short)0, ELEVEN_G, (short)0, out, outOff);
            isPoint(out, outOff, ELEVEN_G, (short)0, true);
            s[31]--;
            addTo(s, (short)0, ELEVEN_G, (short)0, ELEVEN_G, (short)0, out, outOff);
            isPoint(out, outOff, ELEVEN_G, (short)0, true);
            // -G and 2G: G and -2G with Y made p - Y
            negated(g, s, (short)32);
            negated(MINUS_TWO_G, s, (short)97);
            Util.arrayFillNonAtomic(s, (short)0, (short)32, (byte)0);
            s[31] = (byte)2;
            mul(s, (short)0, s, (short)32, out, outOff);
            isPoint(out, outOff, MINUS_TWO_G, (short)0, false);
            s[31] = (byte)4;
            addTo(s, (short)0, s, (short)32, s, (short)97, out, outOff);
            isPoint(out, outOff, MINUS_TWO_G, (short)0, false);
            Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_N, (short)0, s, (short)0, (short)32);
            s[31]--;
            mul(s, (short)0, s, (short)97, out, outOff);
            isPoint(out, outOff, MINUS_TWO_G, (short)0, false);
            // G with a scalar of the full width: [n - 1]G and [n - 2]G + G are -G
            mul(s, (short)0, g, (short)0, out, outOff);
            isPoint(out, outOff, g, (short)0, true);
            s[31]--;
            addTo(s, (short)0, g, (short)0, g, (short)0, out, outOff);
            isPoint(out, outOff, g, (short)0, true);
            // k = SHA-256("BIP0340/aux") (tagHashAux), then m = SHA-256(tagHashNonce || tagHashNonce): [k](2G) at
            // s[162..227), then [k - 1](2G) + 2G, [2k - 2]G + 2G and [2k]G held to it, and the same with m. k is above n/2,
            // so the sum makes 2k - 2 - n; k's last byte is 90 and that sum's DD, m's A8 and 2m - 2's 4E: neither k - 1
            // nor 2k borrows or carries, nor m - 1 nor 2m
            Util.arrayCopyNonAtomic(tagHashAux, (short)0, s, (short)0, (short)32);
            for (short j = 0; ; j++) {
                mul(s, (short)0, s, (short)97, s, (short)162);
                s[31]--;
                addTo(s, (short)0, s, (short)97, s, (short)97, out, outOff);
                isPoint(out, outOff, s, (short)162, false);
                addModN(s, (short)0, s, (short)0, s, (short)0);
                addTo(s, (short)0, g, (short)0, s, (short)97, out, outOff);
                isPoint(out, outOff, s, (short)162, false);
                s[31] += 2;
                mul(s, (short)0, g, (short)0, out, outOff);
                isPoint(out, outOff, s, (short)162, false);
                if (j != (short)0) break;
                taggedHash(tagHashNonce, s, (short)0, (short)0, s, (short)0);
            }
        } finally {
            tmpPriv.setG(g, (short)0, (short)65);
        }
    }

    /** The 65-byte point p at out[off], with its Y made p - Y. */
    private static void negated(byte[] p, byte[] out, short off) {
        Util.arrayCopyNonAtomic(p, (short)0, out, off, (short)65);
        sub32(Secp256k1.SECP256K1_P, (short)0, out, (short)(off + 33), out, (short)(off + 33));
    }

    /** X and Y of the point at want[wantOff] (04 || X || Y) at out[outOff + 1], or 6F00; where `negated`, of its negative (out's Y is made p - Y here, then held to want's). */
    private static void isPoint(byte[] out, short outOff, byte[] want, short wantOff, boolean negated) {
        if (negated) sub32(Secp256k1.SECP256K1_P, (short)0, out, (short)(outOff + 33), out, (short)(outOff + 33));
        if (Util.arrayCompare(out, (short)(outOff + 1), want, (short)(wantOff + 1), (short)64) != (byte)0) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
    }

    /** [11]G, 04 || X || Y, for checkFraming: Y odd, and not the root (x^3 + 7)^((p+1)/4). */
    private static final byte[] ELEVEN_G = {
        (byte)0x04,
        (byte)0x77,(byte)0x4A,(byte)0xE7,(byte)0xF8,(byte)0x58,(byte)0xA9,(byte)0x41,(byte)0x1E,
        (byte)0x5E,(byte)0xF4,(byte)0x24,(byte)0x6B,(byte)0x70,(byte)0xC6,(byte)0x5A,(byte)0xAC,
        (byte)0x56,(byte)0x49,(byte)0x98,(byte)0x0B,(byte)0xE5,(byte)0xC1,(byte)0x78,(byte)0x91,
        (byte)0xBB,(byte)0xEC,(byte)0x17,(byte)0x89,(byte)0x5D,(byte)0xA0,(byte)0x08,(byte)0xCB,
        (byte)0xD9,(byte)0x84,(byte)0xA0,(byte)0x32,(byte)0xEB,(byte)0x6B,(byte)0x5E,(byte)0x19,
        (byte)0x02,(byte)0x43,(byte)0xDD,(byte)0x56,(byte)0xD7,(byte)0xB7,(byte)0xB3,(byte)0x65,
        (byte)0x37,(byte)0x2D,(byte)0xB1,(byte)0xE2,(byte)0xDF,(byte)0xF9,(byte)0xD6,(byte)0xA8,
        (byte)0x30,(byte)0x1D,(byte)0x74,(byte)0xC9,(byte)0xC9,(byte)0x53,(byte)0xC6,(byte)0x1B
    };

    /** -[2]G, 04 || X || Y, for checkFraming: X odd, Y odd, and not the root. */
    private static final byte[] MINUS_TWO_G = {
        (byte)0x04,
        (byte)0xC6,(byte)0x04,(byte)0x7F,(byte)0x94,(byte)0x41,(byte)0xED,(byte)0x7D,(byte)0x6D,
        (byte)0x30,(byte)0x45,(byte)0x40,(byte)0x6E,(byte)0x95,(byte)0xC0,(byte)0x7C,(byte)0xD8,
        (byte)0x5C,(byte)0x77,(byte)0x8E,(byte)0x4B,(byte)0x8C,(byte)0xEF,(byte)0x3C,(byte)0xA7,
        (byte)0xAB,(byte)0xAC,(byte)0x09,(byte)0xB9,(byte)0x5C,(byte)0x70,(byte)0x9E,(byte)0xE5,
        (byte)0xE5,(byte)0x1E,(byte)0x97,(byte)0x01,(byte)0x59,(byte)0xC2,(byte)0x3C,(byte)0xC6,
        (byte)0x5C,(byte)0x3A,(byte)0x7B,(byte)0xE6,(byte)0xB9,(byte)0x93,(byte)0x15,(byte)0x11,
        (byte)0x08,(byte)0x09,(byte)0xCD,(byte)0x9A,(byte)0xCD,(byte)0x99,(byte)0x2F,(byte)0x1E,
        (byte)0xDC,(byte)0x9B,(byte)0xCE,(byte)0x55,(byte)0xAF,(byte)0x30,(byte)0x17,(byte)0x05
    };

    /** The key agreement's answer is 65 bytes, 04 || X || Y, or the card uses nothing of it. */
    private static void framed(short len, byte[] b, short off) {
        if (len != (short)65 || b[off] != (byte)0x04) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
    }

    /** The temporary key's curve, set again where the key is in RAM: a deselect or rest() clears it there. */
    private void ready() {
        if (keyInRam) Secp256k1.curve(tmpPriv);
    }

    /**
     * The temporary key and both key agreements put back to a public scalar,
     * G's x, after each call that gave them a secret one (a nonce, a blinding
     * factor), on every way out: a key agreement may keep a copy of the key it
     * was given, so the key alone is not enough. Where the key is in RAM it is
     * then cleared (and the curve with it, which ready() sets again); where
     * the chip gave only a persistent key, the curve is kept.
     */
    private void rest() {
        ready();
        tmpPriv.setS(Secp256k1.SECP256K1_G, (short) 1, (short) 32);
        ecdh.init(tmpPriv);
        gm.init(tmpPriv);
        if (keyInRam) tmpPriv.clearKey();
    }

    /**
     * Compute a mod n in-place (a is 32 bytes, big-endian).
     *
     * If a ≥ n, subtracts n (at most once: a 256-bit number is under 2n), by
     * a mask, the same work either way.
     */
    static void reduceModN(byte[] a, short aOff) {
        short atLeastN = (short)(under(a, aOff, Secp256k1.SECP256K1_N, (short)32) ^ 1);
        subIf(a, aOff, Secp256k1.SECP256K1_N, atLeastN, (short)32);
    }

    /**
     * out = (a + b) mod n   — 256-bit inputs, 256-bit output.
     * All buffers are big-endian.
     */
    static void addModN(byte[] a, short aOff, byte[] b, short bOff,
                        byte[] out, short outOff) {
        // the sum and whether it is n or more in one pass (the carry of the sum, and the borrow of the sum less n), and n
        // taken off by a mask: the same work for every a and b
        short carry = 0, borrow = 0;
        for (short i = 31; i >= 0; i--) {
            short sum = (short)((a[(short)(aOff+i)] & 0xFF)
                              + (b[(short)(bOff+i)] & 0xFF)
                              + carry);
            out[(short)(outOff+i)] = (byte)sum;
            carry = (short)((sum >> 8) & 1);
            short d = (short)((sum & 0xFF) - (Secp256k1.SECP256K1_N[i] & 0xFF) - borrow);
            borrow = (short)((d >> 8) & 1);
        }
        // n off where the sum passed 2^256 (carry), or its low bytes are n or more (no borrow)
        subIf(out, outOff, Secp256k1.SECP256K1_N, (short)(carry | (borrow ^ 1)), (short)32);
    }

    /**
     * v −= m where `take` is 1, and nothing where it is 0: m under a mask of
     * all ones or all zeros, so the same subtraction runs either way.
     */
    private static void subIf(byte[] v, short off, byte[] m, short take, short len) {
        short mask = (short)(-take & 0xFF);
        short borrow = 0;
        for (short i = (short)(len - 1); i >= 0; i--) {
            short d = (short)((v[(short)(off+i)] & 0xFF) - (m[i] & mask) - borrow);
            v[(short)(off+i)] = (byte)d;
            borrow = (short)((d >> 8) & 1);
        }
    }

    /**
     * 1 where the `len` bytes at v[off] are under the `len` of m, 0 where not:
     * the borrow out of v − m, every column worked, no early answer.
     */
    private static short under(byte[] v, short off, byte[] m, short len) {
        short borrow = 0;
        for (short i = (short)(len - 1); i >= 0; i--) {
            short d = (short)((v[(short)(off+i)] & 0xFF) - (m[i] & 0xFF) - borrow);
            borrow = (short)((d >> 8) & 1);
        }
        return borrow;
    }

    /**
     * a = b where `take` is 1, a kept where it is 0, 32 bytes, by a mask:
     * every byte of both read and written either way.
     */
    private static void choose(byte[] a, short aOff, byte[] b, short bOff, short take) {
        byte mask = (byte)(-take);
        for (short i = 0; i < 32; i++) {
            byte x = a[(short)(aOff+i)];
            a[(short)(aOff+i)] = (byte)(x ^ ((x ^ b[(short)(bOff+i)]) & mask));
        }
    }

    /**
     * out = a * b mod n   — schoolbook 256×256 → 512-bit product,
     * then two-level DELTA reduction.
     *
     * DELTA = 2^256 mod n ≈ 2^128.
     * 2^256 ≡ DELTA (mod n)
     *
     * For 512-bit product p = p_hi·2^256 + p_lo:
     *   p mod n = (p_hi·DELTA + p_lo) mod n
     *
     * p_hi·DELTA ≤ (n-1)·DELTA < n·2^128 → 384 bits → split again:
     *   = q_hi·2^256 + q_lo  (q_hi < DELTA < 2^128)
     *   q_hi·DELTA < 2^128·2^128 = 2^256 < 2n
     *
     * So two iterations bring us to < 2n; final subtractIfGe finishes.
     *
     * Allocates nothing: all 288 bytes of temporary storage come from the
     * caller-supplied `work` buffer (see WORK_LEN and the layout map at the
     * top of the class). `work` must not overlap `out`, `a` or `b`.
     *
     * @param a       256-bit input A (32 bytes, big-endian)
     * @param aOff    offset of A
     * @param b       256-bit input B (32 bytes, big-endian)
     * @param bOff    offset of B
     * @param out     32-byte output buffer
     * @param outOff  offset in output buffer
     * @param work    scratch buffer, ≥ WORK_LEN bytes from workOff
     * @param workOff offset of the scratch region
     */
    static void mulModN(byte[] a, short aOff,
                        byte[] b, short bOff,
                        byte[] out, short outOff,
                        byte[] work, short workOff) {
        mul256x256(a, aOff, b, bOff, work, (short)(workOff + W_PROD));
        reduce512toModN(work, (short)(workOff + W_PROD),
                        out, outOff,
                        work, (short)(workOff + W_RED));
    }

    /**
     * Schoolbook 256×256 → 512-bit unsigned multiplication.
     * a[32], b[32] → out[64], all big-endian.
     *
     * The same work for every input (D21). The operands are secret: the
     * nonce and the key pass through here, and AUTH lets any reader ask for
     * as many signatures as it likes and time each one. So there is no
     * branch and no loop whose length depends on a byte of either operand:
     * a row of zeros is multiplied like any other row, and a row's carry is
     * written, not propagated. It can be written: the rows run from the low
     * byte up, row i writes columns i+1 .. i+32, and no row before it has
     * written column i, so the carry out of row i (at most 255) is the whole
     * of column i.
     */
    private static void mul256x256(byte[] a, short aOff,
                                   byte[] b, short bOff,
                                   byte[] out, short outOff) {
        // JavaCard has no int, so everything below is short arithmetic.
        // Per step: ai*bj + cur + carry <= 255*255 + 255 + 255 = 65535, i.e.
        // exactly 16 bits. A signed short holds those bits (it may read as
        // negative); masking recovers the true byte values, so the result is
        // exact. High byte is taken with (x >> 8) & 0xFF — an arithmetic
        // shift on the sign-extended value still yields the correct byte.
        Util.arrayFillNonAtomic(out, outOff, (short)64, (byte)0);
        for (short i = 31; i >= 0; i--) {
            short ai = (short)(a[(short)(aOff+i)] & 0xFF);
            short carry = 0;
            for (short j = 31; j >= 0; j--) {
                short bj  = (short)(b[(short)(bOff+j)] & 0xFF);
                short pos = (short)(outOff + i + j + 1);
                short cur = (short)(out[pos] & 0xFF);
                short prod = (short)((short)(ai * bj) + cur + carry);
                out[pos]  = (byte)(prod & 0xFF);
                carry     = (short)((prod >> 8) & 0xFF);
            }
            // column i is untouched until now: the carry is all of it
            out[(short)(outOff + i)] = (byte) carry;
        }
    }

    /**
     * out[64] = a[32] × DELTA: the schoolbook product of `mul256x256`, over
     * DELTA's seventeen significant bytes only. Its first DELTA_FROM bytes
     * are zero (2^256 − n is a 129-bit number), and multiplying by them was
     * nearly half of every signature's time: three of the four products a
     * signature makes are by DELTA. `init()` checks the zeros are there.
     */
    private static final short DELTA_FROM = (short) 15;
    private static void mulByDelta(byte[] a, short aOff, byte[] out, short outOff, short from) {
        // the rows from `from` to 31. `from` is fixed at each call, by how many
        // leading bytes of `a` the reduction's bounds make zero for every
        // value: 0 for p_hi, DELTA_FROM for t_hi, 31 for t2_hi
        Util.arrayFillNonAtomic(out, outOff, (short)64, (byte)0);
        for (short i = 31; i >= from; i--) {
            // no early way out for a zero byte of `a`, which is secret (D21): see mul256x256
            short ai = (short)(a[(short)(aOff+i)] & 0xFF);
            short carry = 0;
            for (short j = 31; j >= DELTA_FROM; j--) {
                short bj  = (short)(DELTA[j] & 0xFF);
                short pos = (short)(outOff + i + j + 1);
                short cur = (short)(out[pos] & 0xFF);
                short prod = (short)((short)(ai * bj) + cur + carry);
                out[pos]  = (byte)(prod & 0xFF);
                carry     = (short)((prod >> 8) & 0xFF);
            }
            /* the carry out of the last column written: row i writes columns
             * i+DELTA_FROM+1 .. i+32, the row before it ended at column
             * i+DELTA_FROM+1, so column i+DELTA_FROM is untouched and the
             * carry (at most 255) is all of it */
            out[(short)(outOff + i + DELTA_FROM)] = (byte) carry;
        }
    }

    /**
     * Reduce a 512-bit value (64 bytes, big-endian) to 256-bit mod n.
     * Uses depth-2 DELTA reduction.  Result in out[32].
     *
     * Allocates nothing: needs 224 bytes of scratch from work[workOff..].
     * `work` must not overlap `p` or `out`.
     */
    private static void reduce512toModN(byte[] p, short pOff,
                                        byte[] out, short outOff,
                                        byte[] work, short workOff) {
        short tOff  = (short)(workOff + W_T);   // 64 bytes
        short t2Off = (short)(workOff + W_T2);  // 64 bytes
        short hiOff = (short)(workOff + W_HI);  // 32 bytes

        // p = p_hi * 2^256 + p_lo
        // Step 1: t = p_hi * DELTA  (256×32-eff → 256 bits after mod-n reduction)
        // t fits in 48 bytes worst-case, but we compute mod-n in two steps.
        mulByDelta(p, pOff, work, tOff, (short)0);
        // t = (p_hi * DELTA)_hi * 2^256 + (p_hi * DELTA)_lo
        // Apply identity again to t_hi part. t < 2^256 * 2^129 = 2^385, so
        // t_hi < 2^129: its first DELTA_FROM bytes are zero for every p, and
        // the rows of those bytes are not worked
        mulByDelta(work, tOff, work, t2Off, DELTA_FROM);
        // t2_hi * DELTA < 2^128 * 2^128 = 2^256 < 2n → directly add, then subtract
        // Sum = t2_lo + t[32..63] + p[pOff+32..pOff+63]
        // But t2_hi is tiny (< 2^127), its contribution via DELTA:
        //   t2_hi * DELTA < 2^256 fits in 32 bytes.
        mulSmallByDelta(work, t2Off, work, hiOff, work, (short)(workOff + W_SUB));
        // Now sum = hiContrib + t2[32..63] + t[32..63] + p[32..63]
        short carry = add4x32(work, hiOff,
                work, (short)(t2Off+32),
                work, (short)(tOff+32),
                p, (short)(pOff+32),
                out, outOff);
        // The four-term sum can exceed 2^256. Each overflow unit is a dropped
        // 2^256, and 2^256 ≡ DELTA (mod n), so fold it back rather than losing
        // it: carry·DELTA added (carry 0 to 3), and the one carry that can come
        // of that added the same way. The second cannot carry: after an
        // overflow the value is under 3·DELTA < 2^131, and DELTA more is far
        // under 2^256. Two passes, every column, whatever the carries.
        addDelta(out, outOff, addDelta(out, outOff, carry));
        // Final reduction: the value is under 2^256 < 2n, so n is taken off
        // once at most, by a mask
        reduceModN(out, outOff);
    }

    /**
     * v += m·DELTA over all 32 columns, m from 0 to 3: the carry out of the
     * top (0 or 1 for the sums this is given).
     */
    private static short addDelta(byte[] v, short off, short m) {
        short carry = 0;
        for (short i = 31; i >= 0; i--) {
            short s = (short)((v[(short)(off+i)] & 0xFF) + (short)(m * (DELTA[i] & 0xFF)) + carry);
            v[(short)(off+i)] = (byte)s;
            carry = (short)((s >> 8) & 0xFF);
        }
        return carry;
    }

    /**
     * out = a * DELTA, truncated to 256 bits.
     *
     * <b>Requires a &lt; 2^127.</b> DELTA is a 129-bit value, so a·DELTA is up to
     * 385 bits in general and would NOT fit in the 32-byte output — the earlier
     * claim that "the top 32 bytes are all zero because a &lt; 2^256 and delta &lt;
     * 2^128" was simply false. The precondition holds only because the sole
     * caller passes the high half of t2, which the depth-2 DELTA reduction
     * bounds far below 2^127.
     *
     * The precondition is <i>enforced</i>, not assumed: if the top 32 bytes of
     * the 512-bit product are non-zero the value did not fit, and we throw
     * rather than hand a silently truncated result back into the reduction.
     *
     * The bound the reduction gives is far tighter: t2 < 2^129 · 2^129, so its
     * high half is under 2^2 and only its last byte can be non-zero. That byte
     * is the one row worked; the other 31 bytes of `a`, and the top half of
     * the product, are OR-ed together over every byte whatever their values,
     * and one check at the end refuses a value that is not zero.
     *
     * Unlike the engines' framing check, this one cannot be asked once ahead of use:
     * it is an assertion about an intermediate of <i>this</i> multiplication,
     * not a fixed property of the card. It is unreachable for every input the
     * depth-2 reduction can produce — reaching it means the arithmetic above is
     * wrong, in which case a spent slot is the smaller loss.
     *
     * @param a       operand, 32 bytes big-endian at a[aOff..aOff+31]
     * @param out     32-byte output buffer
     * @param work    scratch, ≥ 64 bytes from workOff; must not overlap a or out
     */
    private static void mulSmallByDelta(byte[] a, short aOff,
                                        byte[] out, short outOff,
                                        byte[] work, short workOff) {
        mulByDelta(a, aOff, work, workOff, (short)31);
        short over = 0;
        for (short i = 0; i < 31; i++) {
            over = (short)(over | (a[(short)(aOff + i)] & 0xFF));
        }
        for (short i = 0; i < 32; i++) {
            over = (short)(over | (work[(short)(workOff + i)] & 0xFF));
        }
        Util.arrayCopy(work, (short)(workOff + 32), out, outOff, (short)32);
        if (over != 0) ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
    }

    /**
     * out = low 256 bits of (a + b + c + d), four 32-byte big-endian terms.
     *
     * This does NOT reduce mod n. It returns the carry out of bit 255, which the
     * caller MUST fold back as carry·DELTA (each dropped 2^256 is worth DELTA
     * mod n). Discarding the return value silently corrupts the reduction.
     *
     * @return carry out of bit 255 (0..3 for four 256-bit terms)
     */
    private static short add4x32(byte[] a, short aOff,
                                 byte[] b, short bOff,
                                 byte[] c, short cOff,
                                 byte[] d, short dOff,
                                 byte[] out, short outOff) {
        // Max per step: 4*255 + carry <= 1024, well inside a signed short.
        short carry = 0;
        for (short i = 31; i >= 0; i--) {
            short s = (short)((short)(a[(short)(aOff+i)] & 0xFF)
                            + (short)(b[(short)(bOff+i)] & 0xFF)
                            + (short)(c[(short)(cOff+i)] & 0xFF)
                            + (short)(d[(short)(dOff+i)] & 0xFF)
                            + carry);
            out[(short)(outOff+i)] = (byte)(s & 0xFF);
            carry = (short)((s >> 8) & 0xFF);
        }
        // The caller MUST fold this carry back in: each unit of carry is a
        // dropped 2^256, which is worth DELTA (mod n). Returning it instead of
        // discarding it fixes mulModN for sums that overflow 256 bits.
        return carry;
    }

    /**
     * out = a + b for 32-byte big-endian values.
     *
     * @return carry out of the most significant byte (0 or 1)
     */
    private static short add32(byte[] a, short aOff,
                               byte[] b, short bOff,
                               byte[] out, short outOff) {
        short carry = 0;
        for (short i = 31; i >= 0; i--) {
            short s = (short)((short)(a[(short)(aOff+i)] & 0xFF)
                            + (short)(b[(short)(bOff+i)] & 0xFF)
                            + carry);
            out[(short)(outOff+i)] = (byte)(s & 0xFF);
            carry = (short)((s >> 8) & 0xFF);
        }
        return carry;
    }

    // ── e·d mod n on the chip's RSA engine (D216) ─────────────────────────

    /**
     * Whether the chip's RSA held to the squaring's known answers at the set-up life's first SET_CARD (`chipMulInit`, run at
     * the first SET_CARD after the install or after a RESET_CARD: D233): worked out there and nowhere else, false until the
     * first. A signature never writes it (D228). Where it is true the signature's e·d is made by `mulModNChip`, else by `mulModN`.
     */
    boolean chipMul;
    /** The chip's RSA and its key for squaring mod n: made at the first SET_CARD (not in the constructor: the chip's first create is kept light), and never again. */
    private Cipher rsaC;
    private RSAPrivateKey sqKey;
    private static final byte[] TWO = { (byte) 2 };
    /** G's x times G's y mod n, for the squaring's test: a full-width product, whose two squarings borrow and carry. */
    private static final byte[] KAT_GXGY = {
        (byte)0x80,(byte)0x57,(byte)0x14,(byte)0xA2,(byte)0x52,(byte)0xD0,(byte)0xC0,(byte)0xB5,
        (byte)0x89,(byte)0x10,(byte)0x90,(byte)0x7E,(byte)0x85,(byte)0xB5,(byte)0xB8,(byte)0x01,
        (byte)0xFF,(byte)0xF6,(byte)0x10,(byte)0xA3,(byte)0x6B,(byte)0xDF,(byte)0x46,(byte)0x84,
        (byte)0x7A,(byte)0x4B,(byte)0xF5,(byte)0xD9,(byte)0xAE,(byte)0x2D,(byte)0x10,(byte)0xED
    };

    /**
     * The squaring key and its known answers, at the first SET_CARD of a set-up life (D233): modulus n padded to 64 bytes (32 zeros before it: the chip's
     * RSA is of its modulus' length and takes an input under it, and answers garbage, with no error, for one at or over it),
     * exponent 2; then 3^2 = 9, (n - 1)^2 = 1, and three products by `mulModNChip` whole (D219): G's x times G's y, G's y times n less
     * G's x (n less the first), and n - 1 times n - 2 (2), whose squarings are eight distinct numbers of the full width, each worked
     * out beforehand. True where all hold; false on any wrong answer or any exception (a chip that refuses
     * the key, the room for it or the use), and `chipMul` is then false: the signature is made as before. `work` and `sc` are
     * the room, and are cleared after.
     */
    boolean chipMulInit() {
        try {
            if (rsaC == null) rsaC = Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false);
            if (sqKey == null) sqKey = (RSAPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_RSA_PRIVATE, KeyBuilder.LENGTH_RSA_512, false);
            Util.arrayFillNonAtomic(work, (short)0, (short)32, (byte)0);
            Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_N, (short)0, work, (short)32, (short)32);
            sqKey.setModulus(work, (short)0, (short)64);
            sqKey.setExponent(TWO, (short)0, (short)1);
            Util.arrayFillNonAtomic(work, (short)0, (short)128, (byte)0);
            work[63] = (byte)3;
            square(work, (short)0, (short)64);
            if (work[127] != (byte)9 || !allZero(work, (short)64, (short)63)) return false;
            Util.arrayFillNonAtomic(work, (short)0, (short)128, (byte)0);
            // n - 1: n's last byte, 41, less one
            Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_N, (short)0, work, (short)32, (short)32);
            work[63]--;
            square(work, (short)0, (short)64);
            if (work[127] != (byte)1 || !allZero(work, (short)64, (short)63)) return false;
            // three products, each with squarings of its own, of full-width operands (D219): a·b for a = G's x, b = G's y; for
            // a = G's y, b = n - G's x (the product is n less the first); and for a = n - 1, b = n - 2 (the product is 2).
            // Their squared values, a + b and |a - b|, are six different numbers, all but one at or above 2^255.
            for (short r = 0; r < 3; r++) {
                if (r == 0) {
                    Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_G, (short)1, sc, (short)0, (short)32);
                    Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_G, (short)33, sc, (short)32, (short)32);
                    Util.arrayCopyNonAtomic(KAT_GXGY, (short)0, sc, (short)96, (short)32);
                } else if (r == 1) {
                    Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_G, (short)33, sc, (short)0, (short)32);
                    subtractFromN(Secp256k1.SECP256K1_G, (short)1, sc, (short)32);
                    subtractFromN(KAT_GXGY, (short)0, sc, (short)96);
                } else {
                    Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_N, (short)0, sc, (short)0, (short)32);
                    Util.arrayCopyNonAtomic(Secp256k1.SECP256K1_N, (short)0, sc, (short)32, (short)32);
                    sc[31]--;
                    sc[63] -= (byte)2;
                    Util.arrayFillNonAtomic(sc, (short)96, (short)32, (byte)0);
                    sc[127] = (byte)2;
                }
                mulModNChip(sc, (short)0, sc, (short)32, sc, (short)64, work, (short)0);
                if (Util.arrayCompare(sc, (short)64, sc, (short)96, (short)32) != (byte)0) return false;
            }
            return true;
        } catch (RuntimeException e) {
            return false;
        } finally {
            Util.arrayFillNonAtomic(sc, (short)0, (short)128, (byte)0);
            Util.arrayFillNonAtomic(work, (short)0, WORK_LEN, (byte)0);
        }
    }

    private static boolean allZero(byte[] a, short off, short len) {
        byte o = 0;
        for (short i = 0; i < len; i++) o |= a[(short)(off + i)];
        return o == 0;
    }

    /**
     * The chip's squaring mod n: the 64 bytes at work[in] (32 zeros, then a value under n) squared into the 64 at work[out],
     * the value in the last 32. The answer is the modulus' length; any other is refused (6F00).
     */
    private void square(byte[] w, short in, short out) {
        rsaC.init(sqKey, Cipher.MODE_DECRYPT);
        if (rsaC.doFinal(w, in, (short)64, w, out) != (short)64) ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
    }

    /**
     * out = a * b mod n by two squarings on the chip: 4ab = (a + b)^2 - (a - b)^2, and the quarter by two halvings mod n.
     * a and b (32 bytes each) are reduced below n where they stand first, and u = a + b mod n and v = |a - b| are under n, as
     * the chip's RSA wants (at or over its modulus it answers garbage, and no error). Every step is a mask or a fixed loop:
     * the same work for every a and b, as `mulModN`'s. `w` is 256 bytes of room, left holding a + b and |a - b| and their
     * squares: the caller clears it (sign() does).
     */
    void mulModNChip(byte[] a, short aOff, byte[] b, short bOff, byte[] out, short outOff, byte[] w, short wOff) {
        short u = wOff, uo = (short)(wOff + 64), v = (short)(wOff + 128), vo = (short)(wOff + 192);
        reduceModN(a, aOff);
        reduceModN(b, bOff);
        Util.arrayFillNonAtomic(w, wOff, (short)256, (byte)0);
        // u = a + b mod n, and v = |a - b|: both differences, and the one that did not borrow, by a mask
        addModN(a, aOff, b, bOff, w, (short)(u + 32));
        short bw = sub32(a, aOff, b, bOff, w, (short)(v + 32));
        sub32(b, bOff, a, aOff, w, (short)(vo + 32));
        choose(w, (short)(v + 32), w, (short)(vo + 32), bw);
        Util.arrayFillNonAtomic(w, vo, (short)64, (byte)0);
        square(w, u, uo);
        square(w, v, vo);
        // z = u^2 - v^2 mod n = 4ab, then a quarter of it
        short borrow = sub32(w, (short)(uo + 32), w, (short)(vo + 32), w, (short)(uo + 32));
        addIf(w, (short)(uo + 32), Secp256k1.SECP256K1_N, borrow);
        halve(w, (short)(uo + 32), w, v);
        halve(w, v, out, outOff);
    }

    /** v += m where take is 1 (mod 2^256: the carry out is dropped), by a mask. */
    private static void addIf(byte[] v, short off, byte[] m, short take) {
        short mask = (short)(-take & 0xFF), carry = 0;
        for (short i = 31; i >= 0; i--) {
            short s = (short)((v[(short)(off + i)] & 0xFF) + (m[i] & mask) + carry);
            v[(short)(off + i)] = (byte)s;
            carry = (short)((s >> 8) & 1);
        }
    }

    /** out = x / 2 mod n (x under n): x, or x + n where x is odd, shifted right once, the 33rd bit in at the top; `out` is not x's place. */
    private static void halve(byte[] x, short xOff, byte[] out, short outOff) {
        short mask = (short)(-(x[(short)(xOff + 31)] & 1) & 0xFF), carry = 0;
        for (short i = 31; i >= 0; i--) {
            short s = (short)((x[(short)(xOff + i)] & 0xFF) + (Secp256k1.SECP256K1_N[i] & mask) + carry);
            out[(short)(outOff + i)] = (byte)s;
            carry = (short)((s >> 8) & 1);
        }
        short prev = carry;
        for (short i = 0; i < 32; i++) {
            short cur = (short)(out[(short)(outOff + i)] & 0xFF);
            out[(short)(outOff + i)] = (byte)((prev << 7) | (cur >> 1));
            prev = (short)(cur & 1);
        }
    }

    // ── Utility ───────────────────────────────────────────────────────────

    /**
     * out = n − a  (modular negation in Z_n), 32 bytes each, big-endian.
     */
    static void subtractFromN(byte[] a, short aOff, byte[] out, short outOff) {
        sub32(Secp256k1.SECP256K1_N, (short)0, a, aOff, out, outOff);
    }

    /**
     * 256-bit subtraction: out = a − b, and the borrow out of the top (0 where
     * a ≥ b). The borrow of each column is the sign of its difference, taken
     * with a shift, not a branch.
     */
    static short sub32(byte[] a, short aOff,
                               byte[] b, short bOff,
                               byte[] out, short outOff) {
        short borrow = 0;
        for (short i = 31; i >= 0; i--) {
            short diff = (short)((a[(short)(aOff+i)] & 0xFF)
                               - (b[(short)(bOff+i)] & 0xFF)
                               - borrow);
            out[(short)(outOff+i)] = (byte)diff;
            borrow = (short)((diff >> 8) & 1);
        }
        return borrow;
    }

    /**
     * Compare two 32-byte big-endian values, for public values only (a header's
     * work, a point's coordinates against p): it answers at the first byte
     * that differs, and nothing on the signing path uses it.
     * @return negative / 0 / positive as a < b / a == b / a > b.
     */
    static short cmp32(byte[] a, short aOff, byte[] b, short bOff) {
        for (short i = 0; i < 32; i++) {
            short ai = (short)(a[(short)(aOff+i)] & 0xFF);
            short bi = (short)(b[(short)(bOff+i)] & 0xFF);
            if (ai != bi) return (short)(ai - bi);
        }
        return (short)0;
    }


    // ── fedimint-card: begin ──────────────────────────────────────────────
    // The one block this package adds to the file copied from cashucard's applet: the applet's
    // card secret (install), STATE_KEY's HMAC and GEN_KEY's scalar alignment use the signer's
    // RNG and SHA-256 rather than objects of their own.

    /** Fills {@code len} bytes of {@code buf} from the secure RNG. */
    void random(byte[] buf, short off, short len) {
        rng.generateData(buf, off, len);
    }

    /**
     * HMAC-SHA256 (RFC 2104) with a 32-byte key, on the shared SHA-256 instance.
     *
     * @param key   32-byte key at {@code keyOff}
     * @param msg   message, {@code msgLen} bytes at {@code msgOff}
     * @param w     96 bytes of scratch at {@code wOff}: the padded key block, then the inner
     *              hash. The caller wipes it.
     * @param out   receives the 32-byte MAC at {@code outOff}; must not overlap {@code w}
     */
    void hmacSha256(byte[] key, short keyOff,
                    byte[] msg, short msgOff, short msgLen,
                    byte[] w, short wOff,
                    byte[] out, short outOff) {
        short inner = (short) (wOff + 64);
        padKey(key, keyOff, w, wOff, (byte) 0x36);
        sha256.reset();
        sha256.update(w, wOff, (short) 64);
        sha256.doFinal(msg, msgOff, msgLen, w, inner);
        padKey(key, keyOff, w, wOff, (byte) 0x5C);
        sha256.reset();
        sha256.update(w, wOff, (short) 64);
        sha256.doFinal(w, inner, (short) 32, out, outOff);
    }

    /** w[0..63] = (key || 32 zero bytes) XOR pad. */
    private static void padKey(byte[] key, short keyOff, byte[] w, short wOff, byte pad) {
        for (short i = 0; i < 32; i++) {
            w[(short) (wOff + i)] = (byte) (key[(short) (keyOff + i)] ^ pad);
        }
        Util.arrayFillNonAtomic(w, (short) (wOff + 32), (short) 32, pad);
    }

    /**
     * Normalise a scalar that getS wrote at b[off..off+len-1] to exactly 32 big-endian bytes
     * at b[off..off+31]. Accepts 1..32 bytes (right-aligned, zero-padded) and 33 bytes with a
     * leading 0x00 (shifted left). Anything else throws SW_CRYPTO_ERROR. b needs room for len
     * bytes from off.
     *
     * @return 32
     */
    static short alignScalar(byte[] b, short off, short len) {
        if (len == (short) 33 && b[off] == (byte) 0) {
            for (short i = 0; i < 32; i++) {
                b[(short) (off + i)] = b[(short) (off + i + 1)];
            }
            b[(short) (off + 32)] = (byte) 0;
            return (short) 32;
        }
        if (len < (short) 1 || len > (short) 32) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        if (len < (short) 32) {
            short pad = (short) (32 - len);
            // Backwards, byte by byte: overlapping move.
            for (short i = (short) (len - 1); i >= (short) 0; i--) {
                b[(short) (off + pad + i)] = b[(short) (off + i)];
            }
            Util.arrayFillNonAtomic(b, off, pad, (byte) 0);
        }
        return (short) 32;
    }
    // ── fedimint-card: end ────────────────────────────────────────────────
}
