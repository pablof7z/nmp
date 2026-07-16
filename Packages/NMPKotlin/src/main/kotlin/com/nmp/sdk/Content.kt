package com.nmp.sdk

import uniffi.nmp_ffi.FfiBlockKind
import uniffi.nmp_ffi.FfiContentDiagnostic
import uniffi.nmp_ffi.FfiContentDocument
import uniffi.nmp_ffi.FfiContentSyntax
import uniffi.nmp_ffi.FfiInlineNode
import uniffi.nmp_ffi.FfiInlineStyle
import uniffi.nmp_ffi.FfiReferenceOccurrence
import uniffi.nmp_ffi.FfiReferencePlacement
import uniffi.nmp_ffi.FfiReferenceTarget
import uniffi.nmp_ffi.FfiSourceRange
import uniffi.nmp_ffi.parseNostrContent as ffiParseNostrContent

enum class NostrContentSyntax {
    PlainText,
    Markdown,
    ;

    internal fun toFfi(): FfiContentSyntax =
        when (this) {
            PlainText -> FfiContentSyntax.PLAIN_TEXT
            Markdown -> FfiContentSyntax.MARKDOWN
        }

    companion object {
        internal fun from(ffi: FfiContentSyntax): NostrContentSyntax =
            when (ffi) {
                FfiContentSyntax.PLAIN_TEXT -> PlainText
                FfiContentSyntax.MARKDOWN -> Markdown
            }
    }
}

data class NostrContentSourceRange(val start: UInt, val end: UInt) {
    companion object {
        internal fun from(ffi: FfiSourceRange) = NostrContentSourceRange(ffi.start, ffi.end)
    }
}

/** Semantic authored context, not a catalog of Compose components. */
sealed class NostrContentBlockContext {
    object Paragraph : NostrContentBlockContext()

    data class Heading(val level: UByte) : NostrContentBlockContext()

    data class Quote(val depth: UByte) : NostrContentBlockContext()

    data class ListItem(
        val ordered: Boolean,
        val ordinal: ULong?,
        val depth: UByte,
    ) : NostrContentBlockContext()

    data class Code(val language: String?) : NostrContentBlockContext()

    object ThematicBreak : NostrContentBlockContext()

    companion object {
        internal fun from(ffi: FfiBlockKind): NostrContentBlockContext =
            when (ffi) {
                is FfiBlockKind.Paragraph -> Paragraph
                is FfiBlockKind.Heading -> Heading(ffi.level)
                is FfiBlockKind.Quote -> Quote(ffi.depth)
                is FfiBlockKind.ListItem -> ListItem(ffi.ordered, ffi.ordinal, ffi.depth)
                is FfiBlockKind.Code -> Code(ffi.language)
                is FfiBlockKind.ThematicBreak -> ThematicBreak
            }
    }
}

enum class NostrContentInlineStyle {
    Emphasis,
    Strong,
    Strikethrough,
    Code,
    ;

    companion object {
        internal fun from(ffi: FfiInlineStyle): NostrContentInlineStyle =
            when (ffi) {
                FfiInlineStyle.EMPHASIS -> Emphasis
                FfiInlineStyle.STRONG -> Strong
                FfiInlineStyle.STRIKETHROUGH -> Strikethrough
                FfiInlineStyle.CODE -> Code
            }
    }
}

enum class NostrReferencePlacement {
    Inline,
    Standalone,
    ;

    companion object {
        internal fun from(ffi: FfiReferencePlacement): NostrReferencePlacement =
            when (ffi) {
                FfiReferencePlacement.INLINE -> Inline
                FfiReferencePlacement.STANDALONE -> Standalone
            }
    }
}

sealed class NostrReferenceTarget {
    data class Profile(
        val pubkey: String,
        val relayHints: List<String> = emptyList(),
    ) : NostrReferenceTarget()

    data class Event(
        val id: String,
        val authorHint: String? = null,
        val kindHint: UShort? = null,
        val relayHints: List<String> = emptyList(),
    ) : NostrReferenceTarget()

    data class Address(
        val kind: UShort,
        val author: String,
        val identifier: String,
        val relayHints: List<String> = emptyList(),
    ) : NostrReferenceTarget()

    val key: String
        get() =
            when (this) {
                is Profile -> "profile:$pubkey"
                is Event -> "event:$id"
                is Address -> "address:$kind:$author:$identifier"
            }

    companion object {
        internal fun from(ffi: FfiReferenceTarget): NostrReferenceTarget =
            when (ffi) {
                is FfiReferenceTarget.Profile -> Profile(ffi.pubkey, ffi.relayHints)
                is FfiReferenceTarget.Event ->
                    Event(ffi.id, ffi.authorHint, ffi.kindHint, ffi.relayHints)
                is FfiReferenceTarget.Address ->
                    Address(ffi.kind, ffi.author, ffi.identifier, ffi.relayHints)
            }
    }
}

data class NostrReferenceOccurrence(
    val id: ULong,
    val original: String,
    val target: NostrReferenceTarget,
    val source: NostrContentSourceRange,
    val placement: NostrReferencePlacement,
) {
    companion object {
        internal fun from(ffi: FfiReferenceOccurrence) =
            NostrReferenceOccurrence(
                ffi.id,
                ffi.original,
                NostrReferenceTarget.from(ffi.target),
                NostrContentSourceRange.from(ffi.source),
                NostrReferencePlacement.from(ffi.placement),
            )
    }
}

sealed class NostrContentInline {
    abstract val source: NostrContentSourceRange

    data class Text(
        val text: String,
        override val source: NostrContentSourceRange,
        val styles: List<NostrContentInlineStyle>,
    ) : NostrContentInline()

    data class Reference(
        val occurrence: NostrReferenceOccurrence,
        val styles: List<NostrContentInlineStyle>,
    ) : NostrContentInline() {
        override val source: NostrContentSourceRange = occurrence.source
    }

    data class Hashtag(
        val hashtag: String,
        val original: String,
        override val source: NostrContentSourceRange,
        val styles: List<NostrContentInlineStyle>,
    ) : NostrContentInline()

    data class Link(
        val destination: String,
        val label: String,
        override val source: NostrContentSourceRange,
        val styles: List<NostrContentInlineStyle>,
    ) : NostrContentInline()

    data class SoftBreak(override val source: NostrContentSourceRange) : NostrContentInline()

    data class HardBreak(override val source: NostrContentSourceRange) : NostrContentInline()

    companion object {
        internal fun from(ffi: FfiInlineNode): NostrContentInline =
            when (ffi) {
                is FfiInlineNode.Text ->
                    Text(
                        ffi.text,
                        NostrContentSourceRange.from(ffi.source),
                        ffi.styles.map(NostrContentInlineStyle::from),
                    )
                is FfiInlineNode.Reference ->
                    Reference(
                        NostrReferenceOccurrence.from(ffi.occurrence),
                        ffi.styles.map(NostrContentInlineStyle::from),
                    )
                is FfiInlineNode.Hashtag ->
                    Hashtag(
                        ffi.hashtag,
                        ffi.original,
                        NostrContentSourceRange.from(ffi.source),
                        ffi.styles.map(NostrContentInlineStyle::from),
                    )
                is FfiInlineNode.Link ->
                    Link(
                        ffi.destination,
                        ffi.label,
                        NostrContentSourceRange.from(ffi.source),
                        ffi.styles.map(NostrContentInlineStyle::from),
                    )
                is FfiInlineNode.SoftBreak ->
                    SoftBreak(NostrContentSourceRange.from(ffi.source))
                is FfiInlineNode.HardBreak ->
                    HardBreak(NostrContentSourceRange.from(ffi.source))
            }
    }
}

data class NostrContentBlock(
    val id: ULong,
    val context: NostrContentBlockContext,
    val source: NostrContentSourceRange,
    val inlines: List<NostrContentInline>,
)

sealed class NostrContentDiagnostic {
    data class InputTruncated(val originalBytes: ULong, val parsedBytes: ULong) :
        NostrContentDiagnostic()

    data class MalformedReference(
        val original: String,
        val source: NostrContentSourceRange,
    ) : NostrContentDiagnostic()

    companion object {
        internal fun from(ffi: FfiContentDiagnostic): NostrContentDiagnostic =
            when (ffi) {
                is FfiContentDiagnostic.InputTruncated ->
                    InputTruncated(ffi.originalBytes, ffi.parsedBytes)
                is FfiContentDiagnostic.MalformedReference ->
                    MalformedReference(ffi.original, NostrContentSourceRange.from(ffi.source))
            }
    }
}

data class NostrContentDocument(
    val syntax: NostrContentSyntax,
    val blocks: List<NostrContentBlock>,
    val diagnostics: List<NostrContentDiagnostic>,
) {
    val references: List<NostrReferenceOccurrence>
        get() =
            blocks.flatMap { it.inlines }.mapNotNull { inline ->
                (inline as? NostrContentInline.Reference)?.occurrence
            }

    companion object {
        internal fun from(ffi: FfiContentDocument) =
            NostrContentDocument(
                syntax = NostrContentSyntax.from(ffi.syntax),
                blocks =
                    ffi.blocks.map { block ->
                        NostrContentBlock(
                            id = block.id,
                            context = NostrContentBlockContext.from(block.kind),
                            source = NostrContentSourceRange.from(block.source),
                            inlines = block.inlines.map(NostrContentInline::from),
                        )
                    },
                diagnostics = ffi.diagnostics.map(NostrContentDiagnostic::from),
            )
    }
}

fun parseNostrContent(
    content: String,
    syntax: NostrContentSyntax = NostrContentSyntax.PlainText,
): NostrContentDocument = NostrContentDocument.from(ffiParseNostrContent(content, syntax.toFfi()))
