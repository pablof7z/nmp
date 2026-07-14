package com.nmp.ui

import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.painter.Painter
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.nmp.sdk.AuthPhase
import com.nmp.sdk.RelayInformation
import com.nmp.sdk.RelayInformationFreshness
import com.nmp.sdk.SourceStatus

/** Immutable presentation fields copied from one engine-owned NIP-11
 * snapshot. This value is not a cache and never performs acquisition. */
data class NmpRelayPresentation(
    val relay: String,
    val advertisedName: String? = null,
    val advertisedDescription: String? = null,
    /** Exact advertised icon text. NMP UI never dereferences it. */
    val advertisedIcon: String? = null,
    val freshness: RelayInformationFreshness,
    val lastError: String? = null,
) {
    constructor(information: RelayInformation) :
        this(
            relay = information.relay,
            advertisedName = information.document.name,
            advertisedDescription = information.document.description,
            advertisedIcon = information.document.icon,
            freshness = information.freshness,
            lastError = information.lastError,
        )

    val displayName: String
        get() = advertisedName.nonempty() ?: relayHost(relay) ?: relay

    val displayDescription: String
        get() = advertisedDescription.nonempty() ?: "No relay description provided"

    val initials: String
        get() {
            val words = displayName.trim().split(Regex("\\s+")).filter { it.isNotEmpty() }
            return if (words.size >= 2) {
                words.take(2).mapNotNull { it.firstOrNull() }.joinToString("").uppercase()
            } else {
                displayName.take(2).uppercase()
            }
        }

    private fun relayHost(value: String): String? {
        val authority = value.substringAfter("://", "")
            .substringBefore('/')
            .substringBefore('?')
            .substringBefore('#')
            .substringAfterLast('@')
        if (authority.isBlank()) return null
        return if (authority.startsWith("[")) {
            authority.substringAfter('[').substringBefore(']').nonempty()
        } else {
            authority.substringBefore(':').nonempty()
        }
    }
}

/** Caller-owned one-shot operation state. Available stale content remains
 * visible with freshness and last-error evidence kept separately. */
sealed interface NmpRelayInformationState {
    val relay: String

    data class Loading(override val relay: String) : NmpRelayInformationState

    data class Available(val presentation: NmpRelayPresentation) : NmpRelayInformationState {
        override val relay: String = presentation.relay
    }

    data class Unavailable(
        override val relay: String,
        val reason: String? = null,
    ) : NmpRelayInformationState

    val availablePresentation: NmpRelayPresentation?
        get() = (this as? Available)?.presentation

    val displayName: String
        get() =
            availablePresentation?.displayName
                ?: NmpRelayPresentation(relay = relay, freshness = RelayInformationFreshness.Stale).displayName

    val displayDescription: String
        get() =
            when (this) {
                is Loading -> "Relay information is loading"
                is Available -> presentation.displayDescription
                is Unavailable -> reason.nonempty() ?: "Relay information unavailable"
            }

    val informationLabel: String
        get() =
            when (this) {
                is Loading -> "Loading"
                is Available ->
                    if (presentation.freshness == RelayInformationFreshness.Fresh) "Fresh" else "Stale"
                is Unavailable -> "Unavailable"
            }

    val lastError: String?
        get() =
            when (this) {
                is Loading -> null
                is Available -> presentation.lastError
                is Unavailable -> reason.nonempty()
            }

    val initials: String
        get() = availablePresentation?.initials ?: displayName.take(2).uppercase()

    companion object {
        fun available(information: RelayInformation): NmpRelayInformationState =
            Available(NmpRelayPresentation(information))
    }
}

/** Presentation of query-scoped SourceStatus. No URL-global connected,
 * authenticated, health, or reconnecting state is invented here. */
sealed interface NmpRelayRuntimePresentation {
    val label: String

    data object StatusUnavailable : NmpRelayRuntimePresentation {
        override val label = "Status unavailable"
    }

    data object Requesting : NmpRelayRuntimePresentation {
        override val label = "Requesting"
    }

    data object Connecting : NmpRelayRuntimePresentation {
        override val label = "Connecting"
    }

    data object Disconnected : NmpRelayRuntimePresentation {
        override val label = "Disconnected"
    }

    data class AwaitingAuth(val phase: AuthPhase) : NmpRelayRuntimePresentation {
        override val label =
            when (phase) {
                AuthPhase.AwaitingPolicy -> "Awaiting authentication policy"
                AuthPhase.AwaitingSignature -> "Awaiting authentication signature"
            }
    }

    data object AuthDenied : NmpRelayRuntimePresentation {
        override val label = "Authentication denied"
    }

    data object Error : NmpRelayRuntimePresentation {
        override val label = "Connection error"
    }

    companion object {
        fun from(status: SourceStatus?): NmpRelayRuntimePresentation =
            when (status) {
                null -> StatusUnavailable
                SourceStatus.Requesting -> Requesting
                SourceStatus.Connecting -> Connecting
                SourceStatus.Disconnected -> Disconnected
                is SourceStatus.AwaitingAuth -> AwaitingAuth(status.phase)
                SourceStatus.AuthDenied -> AuthDenied
                SourceStatus.Error -> Error
            }
    }
}

/** Controlled icon. The caller supplies an already-resolved Painter after
 * applying app-owned media policy; this function never loads a URL. */
@Composable
fun NmpRelayIcon(
    state: NmpRelayInformationState,
    painter: Painter? = null,
    modifier: Modifier = Modifier,
    size: Dp = 44.dp,
) {
    Surface(
        modifier = modifier.size(size).clearAndSetSemantics { },
        shape = CircleShape,
        color = MaterialTheme.colorScheme.secondaryContainer,
    ) {
        Box(contentAlignment = Alignment.Center) {
            Text(
                text = state.initials,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
                fontWeight = FontWeight.SemiBold,
            )
            if (painter != null) {
                Image(
                    painter = painter,
                    contentDescription = null,
                    modifier = Modifier.size(size).clip(CircleShape),
                )
            }
        }
    }
}

@Composable
fun NmpRelayName(
    state: NmpRelayInformationState,
    modifier: Modifier = Modifier,
) {
    Text(
        text = state.displayName,
        modifier = modifier.semantics { contentDescription = "Relay ${state.displayName}" },
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        fontWeight = FontWeight.SemiBold,
    )
}

@Composable
fun NmpRelayDescription(
    state: NmpRelayInformationState,
    modifier: Modifier = Modifier,
) {
    Text(
        text = state.displayDescription,
        modifier = modifier.semantics { contentDescription = state.displayDescription },
        maxLines = 2,
        overflow = TextOverflow.Ellipsis,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

@Composable
fun NmpRelayRuntimeStatus(
    status: NmpRelayRuntimePresentation,
    modifier: Modifier = Modifier,
) {
    Surface(
        modifier = modifier.semantics { contentDescription = "Relay runtime status: ${status.label}" },
        shape = RoundedCornerShape(999.dp),
        color = MaterialTheme.colorScheme.surfaceVariant,
    ) {
        Text(
            text = status.label,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** Controlled relay list/menu row. It only composes supplied state and a
 * supplied Painter; it cannot acquire metadata or mutate relay connections. */
@Composable
fun NmpRelayListEntry(
    information: NmpRelayInformationState,
    runtime: NmpRelayRuntimePresentation = NmpRelayRuntimePresentation.StatusUnavailable,
    painter: Painter? = null,
    modifier: Modifier = Modifier,
    onClick: (() -> Unit)? = null,
) {
    val rowModifier =
        modifier
            .fillMaxWidth()
            .semantics {
                contentDescription = relayListEntryAccessibilityLabel(information, runtime)
            }
            .let { base -> if (onClick == null) base else base.clickable(onClick = onClick) }

    Surface(
        modifier = rowModifier,
        shape = RoundedCornerShape(16.dp),
        color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.42f),
    ) {
        Row(
            modifier = Modifier.padding(12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.Top,
        ) {
            NmpRelayIcon(state = information, painter = painter)
            Column(
                modifier = Modifier.weight(1f),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                NmpRelayName(state = information)
                NmpRelayDescription(state = information)
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        text = information.informationLabel,
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.width(6.dp))
                    NmpRelayRuntimeStatus(status = runtime)
                }
                information.lastError?.let { error ->
                    Text(
                        text = error,
                        modifier = Modifier.semantics {
                            contentDescription = "Relay information error: $error"
                        },
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

internal fun relayListEntryAccessibilityLabel(
    information: NmpRelayInformationState,
    runtime: NmpRelayRuntimePresentation,
): String =
    listOfNotNull(
        "Relay ${information.displayName}",
        information.displayDescription,
        "Relay information ${information.informationLabel}",
        runtime.label,
        information.lastError?.let { "Relay information error: $it" },
    ).joinToString(". ")

private fun String?.nonempty(): String? = this?.trim()?.takeIf { it.isNotEmpty() }
