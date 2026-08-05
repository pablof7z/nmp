// The tagging door (#1243): compose a reply, a chat reply or a repost by
// naming what you are pointing at, and nothing else. Mirrors Tagging.swift.
//
// This is what #1243 asked for. A native NIP-29 chat app could reach
// NMPGroup.publish for the `h` row but had to hand-build the one row NIP-C7
// owns, because the C7 composer never crossed the FFI. The row it built was
// wrong twice over -- a `q` is NIP-18's QUOTE marker, whose whole purpose is
// keeping the referenced event OUT of the thread -- and nothing caught it,
// which is exactly why schema ownership sits in NMP.
//
// Every door here takes the Row the app is already holding and returns a
// WritePayload, the same value NMPEngine.publish and NMPGroup.publish already
// take. None of them takes a relationship, a marker, a relay hint or an
// author: those are what the door fills, from the row's own tags and its
// verified sources.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiContentPart
import uniffi.nmp_ffi.FfiEventBuilder
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.chat as ffiChat
import uniffi.nmp_ffi.chatReply as ffiChatReply
import uniffi.nmp_ffi.replyTo as ffiReplyTo
import uniffi.nmp_ffi.repost as ffiRepost
import uniffi.nmp_ffi.withContent as ffiWithContent

internal fun Row.toFfi(): FfiRow = FfiRow(id, pubkey, createdAt, kind, tags, content, sig, sources)

internal fun FfiEventBuilder.toPayload(): WritePayload =
    WritePayload.Event(kind, tags, content, createdAt)

/**
 * One piece of a message body.
 *
 * Bech32 appears in what a reader SEES and nowhere else -- that is the user
 * boundary (`docs/internals/conventions/bech32-boundary.md`). Every input here
 * is the decoded form: [Person.pubkey] is 64-char hex like every other key in
 * this package, and [Quote] names the [Row] the app is already holding. The
 * `nostr:npub1...`/`nostr:nevent1...` token is produced from those, which is
 * exactly the pairing this type exists to keep honest.
 */
sealed interface ContentPart {
    /**
     * Literal text, rendered verbatim and emitting no rows. A `nostr:` URI
     * typed into this arm is just characters: nothing parses it, so it emits
     * nothing. Name the person or the event instead.
     */
    data class Text(val text: String) : ContentPart

    /**
     * Somebody named inline. Renders `nostr:npub1...` and emits their `p` row.
     *
     * [relay] is where a reader should look for them, when the app knows -- a
     * person's relay is an outbox fact (NIP-65) no schema owner can reach, so
     * `null` leaves the slot honestly empty rather than guessing. Stating one
     * reaches both halves: the rendering becomes `nostr:nprofile1...` carrying
     * that relay, and the `p` row's hint cell carries the same value.
     */
    data class Person(val pubkey: String, val relay: String?) : ContentPart

    /**
     * An event named inline. Renders `nostr:nevent1...` and emits its NIP-18
     * `q` row, hinted from the row's own verified sources.
     *
     * It is a QUOTE and never a thread reply: NIP-18's `q` exists precisely so
     * "quote reposts are not pulled and included as replies in threads".
     * Replying is [chatReply]/[replyTo], which point with `e`.
     */
    data class Quote(val target: Row) : ContentPart
}

internal fun ContentPart.toFfi(): FfiContentPart =
    when (this) {
        is ContentPart.Text -> FfiContentPart.Text(text)
        is ContentPart.Person -> FfiContentPart.Person(pubkey, relay)
        is ContentPart.Quote -> FfiContentPart.Quote(target.toFfi())
    }

/**
 * State what this draft SAYS, and emit the rows its inline references need,
 * from one call.
 *
 * A composed draft is content-free until the app says what it says, so this is
 * how a draft from one of these doors becomes a message. It takes the message
 * in PIECES rather than as a finished string because a piece naming a person
 * or an event produces both halves of that reference -- the
 * `nostr:npub1...`/`nostr:nevent1...` a reader sees and the `p`/`q` row that
 * resolves it -- so the two cannot be written apart. Writing them apart is
 * what #964 found still living in an app: it appended `["p", hex]` by hand and
 * hoped it matched the token it had put in the content, and nothing could
 * catch a disagreement, because from the app's side nothing is missing.
 *
 * The rows land after whatever the composer already stated for its own
 * reasons -- a chat reply's `e` and `p` rows survive intact.
 *
 * A pre-signed payload is returned unchanged: its content is frozen in bytes
 * that were already signed over, so changing it would invalidate the signature
 * rather than edit the message.
 */
fun WritePayload.withContent(content: List<ContentPart>): WritePayload =
    when (this) {
        is WritePayload.Event ->
            nmpRethrowing {
                ffiWithContent(
                    FfiEventBuilder(kind, tags, this.content, createdAt),
                    content.map { it.toFfi() },
                )
            }.toPayload()
        is WritePayload.Signed -> this
    }

/**
 * Compose a top-level NIP-C7 kind:9 chat.
 *
 * The other half of what [chatReply] closed: an app that replies no longer
 * states a kind, but an app sending an ordinary message still stated `kind: 9`
 * itself, because the composer for THAT never crossed the FFI (#964).
 *
 * It composes SCHEMA ONLY, exactly as [chatReply] does -- no `h` row, no
 * notification policy, no routing, and no content. What the message says comes
 * from [withContent], which is also what emits the rows an inline mention or
 * quote needs. A group's `h` row and its relay set come from the group door.
 */
fun chat(): WritePayload = ffiChat().toPayload()

/**
 * Compose the ordinary reply to [target].
 *
 * Two-way and no more: a text note threads through NIP-10, and everything
 * else becomes a NIP-22 comment. The split reads the TARGET's kind, and the
 * root/parent determination underneath reads neither the target's kind nor
 * the kind being composed -- it reads the target's own rows. So a reply
 * composed by an app that believes it is replying to a thread root and one
 * composed by an app that knows better produce the same rows, which is the
 * inversion amethyst#629 shipped and this makes unspellable.
 */
fun replyTo(target: Row): WritePayload = nmpRethrowing { ffiReplyTo(target.toFfi()) }.toPayload()

/**
 * Compose a NIP-C7 kind:9 chat reply to [target].
 *
 * C7 offers its own verb rather than an arm in the general dispatcher because
 * kind:9 must NOT become a NIP-22 comment: NIP-29 clients MUST only fetch
 * kind 9, so a 1111 reply inside a group would be invisible to every one of
 * them. The reply row is `e`, not `q`.
 *
 * It composes SCHEMA ONLY -- no `h` row, no notification policy, no routing.
 * A group's `h` row and its relay set come from [NMPGroup.publish], which
 * takes exactly this value.
 */
fun chatReply(target: Row): WritePayload =
    nmpRethrowing { ffiChatReply(target.toFfi()) }.toPayload()

/**
 * Compose a NIP-18 repost of [target].
 *
 * NIP-18 owns both kinds, so the two-way split happens inside it: a reposted
 * text note is a kind:6 and anything else is a kind:16 that states what it
 * reposted. A caller never picks a kind.
 */
fun repost(target: Row): WritePayload = nmpRethrowing { ffiRepost(target.toFfi()) }.toPayload()
