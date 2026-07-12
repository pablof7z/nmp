# NMP UI north-star component catalogue

Status: exploratory catalogue for issue #141 under epic #75. This is not a promise that 994 public Swift types ship at once. It is the complete opportunity map used to prevent another hand-waved renderer effort. Names will be consolidated when semantic and DX proofs show which concepts should be compound components, styles, or recipes.

## Reading the catalogue

- **Linked primitive or headless contract:** versioned native mechanics and renderer contracts.
- **Source-installed composition:** polished open-code SwiftUI default intended for application ownership and editing.
- **Protocol pack component:** typed semantics plus native renderers for one protocol family.
- **App recipe:** useful complete product flow assembled from public components, but not a universal NMP policy.
- **Gallery proof instrument:** exists to prove parsing, demand, evidence, bounds, overrides, accessibility, or performance.
- **First showcase:** first-party priority for the eventual live Gallery, not one implementation PR.
- **North star:** later breadth after the parser, reference session, source mapping, renderer DX, and native performance seams are proven.

## Count

- 80 families
- 994 unique named candidates
- 173 linked primitives or contracts
- 253 source-installed compositions
- 325 protocol-pack components
- 125 app-owned recipes
- 118 Gallery-only proof instruments

## Actual first implementation slice

The first implementation remains intentionally much smaller than this opportunity map:

1. NostrContent plus source-mapped plaintext and Markdown.
2. ContentPreview for the 29er-style channel row.
3. MentionText, MentionWithAvatar, and MentionPeek.
4. EventChromeCompactRail, EventChromeStandard, and EventChromeQuote.
5. AnimatedReactionToggle with canonical write/query state.
6. ArticleInlineLink, ArticleCard, and ArticleReader.
7. PhotoCard and MediaGrid.
8. ListingCard and ListingDetail.
9. UnknownEvent plus one app-owned custom-kind renderer.
10. EvidenceSheet, RendererOverrideSwitcher, StateLab, and the no-hint naddr proof journey.

## NDK registry parity baseline

The inspected NDK Svelte registry contains 164 generated manifest entries and 206 Svelte views: 84 styled entries, 21 primitive entries, 24 builders/state entries, 6 blocks, 20 icons, and 9 utilities. The parity gate for this catalogue is 84/84 styled capabilities represented, 21/21 primitive families mapped, 24/24 headless behaviors mapped to ordinary NMP query/write machinery or explicitly rejected as app policy, and 6/6 blocks classified as reusable or recipe-only. Cosmetic skins such as neon, glass, Instagram, and Twitter are recipes, not permanent semantic APIs.

The new architecture deliberately does not copy global mutable renderer registration, leaf-owned queries, hidden NDK context, DOM placeholder mounting, raw HTML rendering, media policy embedded in views, or action components that create their own subscriptions.

## Complete list

### Headless renderer contracts

Layer: linked primitive or headless contract. Priority: first showcase. Count: 17.

- `NostrContent`
- `NostrInlineContent`
- `NostrBlockContent`
- `ContentRenderers`
- `RendererPack`
- `RenderPurpose`
- `ReferencePlacement`
- `ContentActions`
- `RenderPath`
- `ResourceState`
- `ReferenceResource`
- `ContentSnapshot`
- `ContentTheme`
- `ContentMotion`
- `ContentAccessibility`
- `MediaPolicy`
- `LinkPolicy`

### Content and document foundation

Layer: linked primitive or headless contract. Priority: first showcase. Count: 26.

- `ContentPreview`
- `InlineContentFlow`
- `ContentBlockStack`
- `SourceMappedText`
- `SelectableDocument`
- `DocumentDecorationLayer`
- `SelectionActionBar`
- `ParagraphBlock`
- `HeadingBlock`
- `BlockQuote`
- `ListBlock`
- `CodeBlock`
- `InlineCode`
- `ThematicBreak`
- `LinkInline`
- `HashtagInline`
- `CustomEmojiInline`
- `LightningInvoiceInline`
- `NostrReferenceInline`
- `NostrReferenceBlock`
- `EmbedContainer`
- `ContentDisclosure`
- `UnsupportedInline`
- `UnsupportedBlock`
- `UnknownEvent`
- `RawEventDisclosure`

### Advanced document blocks

Layer: linked primitive or headless contract. Priority: north star. Count: 6.

- `TableBlock`
- `DefinitionListBlock`
- `FootnoteReference`
- `FootnotePopover`
- `CitationPreviewSlot`
- `ContentComparison`

### Resolution and fallback states

Layer: linked primitive or headless contract. Priority: first showcase. Count: 15.

- `UnresolvedReference`
- `CachedWhileRefreshing`
- `ReferenceShortfall`
- `InvalidReference`
- `UnavailableReference`
- `DeletedEventTombstone`
- `ExpiredEventTombstone`
- `ReplacedEventIndicator`
- `CollapsedEmbed`
- `CyclicReferenceFallback`
- `DepthLimitFallback`
- `HydrationBudgetFallback`
- `AccessRestrictedState`
- `ProvenancePill`
- `EvidenceSheet`

### Advanced access states

Layer: linked primitive or headless contract. Priority: north star. Count: 7.

- `AuthenticationRequiredState`
- `PrivateRecipientUnroutableState`
- `AddressUpdatePulse`
- `InvalidResolvedEvent`
- `HydrationDeferred`
- `NetworkShortfall`
- `RetryControl`

### Identity primitives

Layer: linked primitive or headless contract. Priority: first showcase. Count: 14.

- `Avatar`
- `AvatarGroup`
- `DisplayName`
- `ProfileHandle`
- `PublicKeyLabel`
- `ShortNostrIdentifier`
- `Nip05Badge`
- `ProfileLabel`
- `ProfileBadgeRow`
- `ProfileReferenceButton`
- `AuthorByline`
- `MutedProfileIndicator`
- `BlockedProfileIndicator`
- `SessionIdentityChip`

### Identity protocol values

Layer: typed protocol pack component. Priority: north star. Count: 9.

- `ExternalIdentityBadge`
- `ExternalIdentityLink`
- `UserStatusPill`
- `ProfileStatusBubble`
- `ProfileStatusLine`
- `MusicStatus`
- `ProfileBadgeStrip`
- `ProofOfWorkBadge`
- `IdentityVerificationDetails`

### Mentions and profile compositions

Layer: source-installed native composition. Priority: first showcase. Count: 20.

- `UserAvatarName`
- `MentionText`
- `MentionWithAvatar`
- `MentionPill`
- `MentionPeek`
- `ProfileChip`
- `UserListItem`
- `UserCardCompact`
- `UserCardClassic`
- `UserCardLandscape`
- `UserCardPortrait`
- `ProfileHero`
- `ProfileSummary`
- `ProfileSearchResult`
- `UserPickerRow`
- `FollowButton`
- `FollowButtonPill`
- `MuteButton`
- `ProfileSheet`
- `ProfileShareCard`

### Identity recipes

Layer: app-owned product recipe. Priority: north star. Count: 10.

- `ProfileBadgeShelf`
- `ProfileExternalIdentities`
- `ProfileRelayHomes`
- `MutualConnections`
- `FollowsYouBadge`
- `NostrPassport`
- `SessionSwitcher`
- `ProfileEditor`
- `ProfileIdentitySheet`
- `SuggestedFollowCard`

### Event anatomy

Layer: linked primitive or headless contract. Priority: first showcase. Count: 26.

- `EventRoot`
- `EventAuthor`
- `EventTimestamp`
- `RelativeTimestamp`
- `EventSubject`
- `EventContext`
- `EventBody`
- `EventMedia`
- `EventActions`
- `EventStatusOverlay`
- `EventEvidenceBadge`
- `EventMetadataLine`
- `RepostAttribution`
- `ReplyContext`
- `ParentEventBreadcrumb`
- `ThreadConnector`
- `AvatarRail`
- `EventBodySlot`
- `EventFooter`
- `EventMenuButton`
- `EventStatusBadge`
- `QuoteFrame`
- `SelectionGutter`
- `PinnedIndicator`
- `EditedIndicator`
- `ExpirationIndicator`

### Event chrome compositions

Layer: source-installed native composition. Priority: first showcase. Count: 33.

- `EventChromeBare`
- `EventChromeInline`
- `EventChromeCompactRail`
- `EventChromeStandard`
- `EventChromeDetail`
- `EventChromeQuote`
- `EventChromeThreadRow`
- `EventChromeChannelPreview`
- `EventChromeNotification`
- `EventChromeSearchResult`
- `RepostAttributionHeader`
- `ReplyContextHeader`
- `BareEventContent`
- `InlineEventEmbed`
- `CompactEventRow`
- `AvatarRailEvent`
- `StandardFeedEvent`
- `ElevatedEventCard`
- `DetailEvent`
- `ImmersiveEvent`
- `QuoteEvent`
- `NestedQuoteEvent`
- `ThreadEventRow`
- `ConversationEvent`
- `ChannelMessage`
- `RepostEvent`
- `NotificationEvent`
- `SearchResultEvent`
- `PinnedEvent`
- `BookmarkEvent`
- `ModerationEvent`
- `UnknownEventCard`
- `EventHero`

### Experimental event chrome

Layer: source-installed native composition. Priority: north star. Count: 12.

- `EventChromeChatBubble`
- `EventChromeMediaOverlay`
- `EventChromeMasonryTile`
- `EventChromeConversationStarter`
- `GroupContextHeader`
- `CommunityContextHeader`
- `ProtectedEventBadge`
- `ExpirationCountdown`
- `ChatBubble`
- `EventCarouselCard`
- `EventMasonryTile`
- `EventPassport`

### Social action primitives

Layer: linked primitive or headless contract. Priority: first showcase. Count: 16.

- `AnimatedReactionToggle`
- `ReactionBurst`
- `ReactionCount`
- `ReplyButton`
- `RepostButton`
- `QuoteButton`
- `ZapButton`
- `ZapAmount`
- `BookmarkButton`
- `ShareButton`
- `FollowStateButton`
- `ReportButton`
- `MoreButton`
- `ActionPendingIndicator`
- `WriteReceiptIndicator`
- `EventActionBar`

### Social action compositions

Layer: source-installed native composition. Priority: first showcase. Count: 28.

- `ReactionButton`
- `ReactionButtonAvatars`
- `ReactionButtonSlack`
- `ReactionPicker`
- `ReactionSummary`
- `ReactionPeopleSheet`
- `ReplyButtonAvatars`
- `RepostButtonAvatars`
- `RepostChoiceMenu`
- `ZapButtonAvatars`
- `CopyNostrURIButton`
- `NostrQRCode`
- `EventMoreMenu`
- `ActionBarCompact`
- `ActionBarStandard`
- `ActionBarDetail`
- `CompactActionBar`
- `FullActionBar`
- `FloatingActionBar`
- `ReactionCluster`
- `ReactionsFacepile`
- `ReactionDetailsSheet`
- `ZapSummary`
- `ZapSheet`
- `RepostAttributionBanner`
- `FollowCallToAction`
- `ParticipantsStrip`
- `EventEngagementSheet`

### Advanced event actions

Layer: source-installed native composition. Priority: north star. Count: 8.

- `EngagementSummary`
- `DiscussableFooter`
- `DeleteEventAction`
- `EditAddressableAction`
- `ReportAction`
- `BookmarkEventAction`
- `EventShareCard`
- `EventSheet`

### Hashtags, links, and lightweight references

Layer: source-installed native composition. Priority: first showcase. Count: 18.

- `Hashtag`
- `HashtagModern`
- `HashtagCardCompact`
- `HashtagCardPortrait`
- `LinkInlineBasic`
- `LinkEmbed`
- `EventReferencePeek`
- `UniversalEntityPeek`
- `InlineAvatarMention`
- `InlineEventReference`
- `InlineAddressReference`
- `InlineNostrURI`
- `InlineInvoice`
- `InlineExternalLink`
- `InlineSubject`
- `InlineContentWarning`
- `InlineUnsupportedReference`
- `ReferenceResolutionIndicator`

### External identifiers and handlers

Layer: typed protocol pack component. Priority: north star. Count: 10.

- `ExternalIdentifierChip`
- `ExternalDiscussionCard`
- `OpenWithHandler`
- `HandlerChoiceSheet`
- `AppHandlerCard`
- `OpenWithMenu`
- `ExternalContentReference`
- `ExternalContentCard`
- `RecommendedHandlerCard`
- `OpenInApplicationSheet`

### Media primitives

Layer: linked primitive or headless contract. Priority: first showcase. Count: 20.

- `MediaFrame`
- `MediaImage`
- `MediaGrid`
- `MediaOverflow`
- `MediaCaption`
- `MediaAltText`
- `SensitiveMediaGate`
- `NostrImage`
- `ImagePlaceholder`
- `ImageMosaic`
- `MediaPager`
- `VideoPoster`
- `VideoControlsShell`
- `AudioControlsShell`
- `AudioWaveform`
- `FileAttachment`
- `AltTextBadge`
- `SensitiveMediaCover`
- `MediaUnavailable`
- `FullscreenMediaViewer`

### Media compositions

Layer: source-installed native composition. Priority: first showcase. Count: 20.

- `MediaBasic`
- `MediaBento`
- `MediaCarousel`
- `MediaLightbox`
- `ImageCardBase`
- `ImageCardHero`
- `ImageCardInstagram`
- `ImageContent`
- `PhotoTile`
- `PhotoCard`
- `PhotoDetail`
- `PhotoInformationSheet`
- `PhotoPost`
- `PhotoGrid`
- `PhotoStory`
- `ImmersivePhoto`
- `GalleryViewer`
- `SensitiveMediaCard`
- `MediaMetadataSheet`
- `CaptionedMediaStack`

### Video, audio, and files

Layer: source-installed native composition. Priority: north star. Count: 20.

- `MediaVideoSlot`
- `VideoInlineEmbed`
- `VideoCardLandscape`
- `VideoCardPortrait`
- `VideoDetail`
- `VideoTextTrackPicker`
- `MediaAudioSlot`
- `VoiceMessageBubble`
- `VoiceMessageReplyPreview`
- `AudioPost`
- `PodcastEpisodeCard`
- `FileCard`
- `FileListItem`
- `FileIntegrityBadge`
- `FilePreviewSlot`
- `FileMetadataCard`
- `FilePreview`
- `FileDownloadAction`
- `ImageMetadataSheet`
- `BlurHashPlaceholder`

### Native media capability recipes

Layer: app-owned product recipe. Priority: north star. Count: 7.

- `MediaUploadButton`
- `MediaUploadCarousel`
- `VoiceRecorder`
- `MediaAttachmentTray`
- `AttachmentTile`
- `AltTextEditor`
- `ContentWarningEditor`

### NIP-23 articles

Layer: typed protocol pack component. Priority: first showcase. Count: 20.

- `ArticleInlineLink`
- `ArticleCardInline`
- `ArticleCardCompact`
- `ArticleCard`
- `ArticleCardPortrait`
- `ArticleCardHero`
- `ArticleCardFeed`
- `ArticleHeader`
- `ArticleByline`
- `ArticleBody`
- `ArticleReader`
- `ArticleReadingProgress`
- `ArticleReadingTime`
- `ArticleUpdatedBadge`
- `ArticleAuthorFooter`
- `ArticleCommentsSummary`
- `ArticleExcerpt`
- `ArticleMetadata`
- `ArticleRevisionIndicator`
- `ArticleSharePreview`

### Advanced article reading

Layer: typed protocol pack component. Priority: north star. Count: 5.

- `ArticleTableOfContents`
- `ArticleFootnotePopover`
- `ArticleReferenceRail`
- `ArticleDraftPreview`
- `ArticleShareCard`

### Article product recipes

Layer: app-owned product recipe. Priority: north star. Count: 6.

- `ArticleSeriesShelf`
- `ReaderAppearanceControls`
- `ArticleEditor`
- `ArticleComposerShell`
- `ArticleWidget`
- `ArticleAnnotationReader`

### NIP-54 wiki basics

Layer: typed protocol pack component. Priority: first showcase. Count: 9.

- `WikiInlineLink`
- `WikiRedlink`
- `WikiCard`
- `WikiTopicHeader`
- `WikiBody`
- `WikiReader`
- `WikiVariantPicker`
- `WikiRedirectBanner`
- `WikiAuthorProvenance`

### Advanced wiki and knowledge

Layer: typed protocol pack component. Priority: north star. Count: 6.

- `WikiVariantComparison`
- `WikiDisambiguationList`
- `WikiBacklinks`
- `WikiMergeRequestCard`
- `WikiMergeDiff`
- `WikiMergeDecisionBar`

### Knowledge product recipes

Layer: app-owned product recipe. Priority: north star. Count: 5.

- `WikiTopicMap`
- `WikiKnowledgeTrail`
- `WikiEditor`
- `WikiEditorShell`
- `KnowledgeCanvas`

### NIP-84 highlights

Layer: typed protocol pack component. Priority: first showcase. Count: 19.

- `HighlightMark`
- `HighlightGutter`
- `HighlightPopover`
- `HighlightCardInline`
- `HighlightCardCompact`
- `HighlightCardFeed`
- `HighlightCardElegant`
- `HighlightCardGrid`
- `HighlightContextExcerpt`
- `HighlightAttributionStack`
- `QuoteHighlightEmbed`
- `OrphanedHighlightFallback`
- `AmbiguousHighlightIndicator`
- `HighlightComposer`
- `HighlightShareCard`
- `HighlightContext`
- `HighlightReaderOverlay`
- `HighlightThread`
- `HighlightAuthorBadge`

### Advanced annotation experiences

Layer: app-owned product recipe. Priority: north star. Count: 9.

- `HighlightCommentThread`
- `OverlappingHighlightSwitcher`
- `AnnotationDensityGutter`
- `TrustedHighlightLayer`
- `CollaborativeReadingMode`
- `AnnotatedReader`
- `HighlightSummary`
- `InlineCommentThread`
- `AnnotationHeatmap`

### Selection and annotation primitives

Layer: linked primitive or headless contract. Priority: first showcase. Count: 11.

- `SelectableContent`
- `TextSelectionToolbar`
- `HighlightGutterMarker`
- `AnnotationCount`
- `QuoteSelectionButton`
- `HighlightComposerSheet`
- `HighlightStylePicker`
- `ScrollToHighlightButton`
- `DocumentOutline`
- `ReferenceAccessibilityRotor`
- `HighlightAccessibilityRotor`

### NIP-99 listings

Layer: typed protocol pack component. Priority: first showcase. Count: 24.

- `ListingInlineLink`
- `ListingCardCompact`
- `ListingCard`
- `ListingCardHero`
- `ListingDetail`
- `ListingMediaGallery`
- `ListingPrice`
- `ListingStatusBadge`
- `ListingCategoryChips`
- `ListingLocation`
- `ListingSellerSummary`
- `ListingMetadataTable`
- `ContactSellerButton`
- `ListingShareCard`
- `ProductGridTile`
- `ProductDetail`
- `ProductImageGallery`
- `PriceLabel`
- `AvailabilityBadge`
- `SellerCard`
- `LocationLabel`
- `ShippingTerms`
- `PurchaseAction`
- `ClassifiedExpirationBanner`

### Listing product recipes

Layer: app-owned product recipe. Priority: north star. Count: 4.

- `ListingDraftPreview`
- `ListingEditor`
- `SellerStorefront`
- `ProductComposerShell`

### Legacy NIP-15 marketplace

Layer: typed protocol pack component. Priority: north star. Count: 7.

- `LegacyStallCard`
- `LegacyProductCard`
- `LegacyProductDetail`
- `LegacyAuctionCard`
- `LegacyBidRow`
- `LegacyCheckoutSummary`
- `LegacyOrderTimeline`

### Lists and starter packs

Layer: typed protocol pack component. Priority: first showcase. Count: 10.

- `FollowPackListItem`
- `FollowPackCompact`
- `FollowPackModern`
- `FollowPackPortrait`
- `FollowPackHero`
- `FollowPackDetail`
- `FollowPackInstallPreview`
- `FollowPackInstallButton`
- `StarterPackCard`
- `ListMemberRow`

### NIP-51 collections

Layer: typed protocol pack component. Priority: north star. Count: 23.

- `MediaStarterPackCard`
- `FollowSetCard`
- `GenericListCard`
- `RelaySetCard`
- `BookmarkSetCard`
- `CurationSetShelf`
- `VideoSetCarousel`
- `PictureSetGrid`
- `InterestSetCloud`
- `EmojiSetGrid`
- `KindMuteSetCard`
- `BadgeSetShelf`
- `AppCurationSet`
- `ReleaseArtifactSet`
- `ListMembershipBadge`
- `WebBookmarkRow`
- `WebBookmarkCard`
- `WebBookmarkDetail`
- `BookmarkListCard`
- `MuteListSummary`
- `PinList`
- `CuratedSetCard`
- `EmojiSetCard`

### List editing recipes

Layer: app-owned product recipe. Priority: north star. Count: 5.

- `AddToListSheet`
- `ListDiffReview`
- `ListEditor`
- `StarterPackDiff`
- `ContentTabs`

### Threads and universal comments

Layer: typed protocol pack component. Priority: first showcase. Count: 17.

- `ParentEventPreview`
- `ThreadBranch`
- `CollapsedReplies`
- `UniversalCommentCard`
- `UniversalCommentComposer`
- `CommentSummary`
- `QuoteRepostCard`
- `ThreadContext`
- `GenericComment`
- `CommentTargetHeader`
- `ShortNote`
- `ShortNoteCard`
- `ShortNoteDetail`
- `NostrReferenceLink`
- `NostrReferencePreview`
- `RepostCard`
- `GenericRepostCard`

### Thread and conversation compositions

Layer: source-installed native composition. Priority: first showcase. Count: 9.

- `ThreadRoot`
- `ConversationTimeline`
- `ChatTranscript`
- `UnreadDivider`
- `ThreadSummary`
- `ReplyPreview`
- `ReplyContextSheet`
- `ConversationHeader`
- `ThreadSheet`

### Public messages and legacy channels

Layer: typed protocol pack component. Priority: north star. Count: 8.

- `NIP7DThreadCard`
- `NIP7DThreadView`
- `PublicMessageCard`
- `PublicMessageRecipientBar`
- `PublicMessageComposer`
- `LegacyChannelCard`
- `LegacyChannelHeader`
- `LegacyChannelMessage`

### Conversation product recipes

Layer: app-owned product recipe. Priority: north star. Count: 3.

- `ThreadMiniMap`
- `ConversationContextStack`
- `ThreadMap`

### Private messaging pack

Layer: typed protocol pack component. Priority: north star. Count: 11.

- `ConversationListRow`
- `DirectMessageHeader`
- `DirectMessageBubble`
- `DirectFileMessageBubble`
- `DirectMessageReplyPreview`
- `DirectMessageReactionBar`
- `DirectMessageDeletedTombstone`
- `DisappearingMessageTimer`
- `MessageDeliveryEvidence`
- `ConversationUnreadDivider`
- `GiftWrappedMessageFailure`

### Private messaging recipes

Layer: app-owned product recipe. Priority: north star. Count: 6.

- `DirectMessageComposer`
- `RecipientPicker`
- `ConversationSafetyRequest`
- `PrivateInboxProblem`
- `ConversationMediaComposer`
- `DirectMessageConversation`

### Calendar events

Layer: typed protocol pack component. Priority: north star. Count: 16.

- `CalendarEventInline`
- `CalendarAgendaRow`
- `CalendarEventCard`
- `CalendarEventDetail`
- `AllDayEventBadge`
- `EventTimeRange`
- `EventLocationRow`
- `ParticipantRoleStack`
- `CalendarCard`
- `CalendarDetail`
- `RSVPButton`
- `RSVPPicker`
- `RSVPSummary`
- `AttendeeStack`
- `CalendarInclusionRequest`
- `AddToCalendarAction`

### Live activities

Layer: typed protocol pack component. Priority: north star. Count: 14.

- `LiveActivityCard`
- `LiveActivityHero`
- `LiveNowBadge`
- `LiveParticipantRoles`
- `LiveStreamSlot`
- `LiveChatMessage`
- `LiveChatReply`
- `PinnedLiveChatMessage`
- `MeetingRoomCard`
- `RoomPresence`
- `StreamPlayerShell`
- `LiveActivityDetail`
- `JoinLiveActivityButton`
- `LiveStatusBanner`

### Event and live product recipes

Layer: app-owned product recipe. Priority: north star. Count: 4.

- `AddToSystemCalendarButton`
- `LiveEventArc`
- `LiveActivityWidgetRecipe`
- `LiveActivityWidget`

### NIP-29 groups

Layer: typed protocol pack component. Priority: north star. Count: 19.

- `GroupChip`
- `GroupCard`
- `GroupHeader`
- `GroupAccessBadges`
- `GroupMembershipBadge`
- `GroupRoleBadge`
- `GroupAdminStack`
- `GroupMemberPreview`
- `GroupJoinButton`
- `GroupLeaveButton`
- `GroupInviteEntry`
- `GroupPostChrome`
- `GroupTimeline`
- `GroupModerationAction`
- `GroupLiveRoom`
- `GroupBadge`
- `GroupMessage`
- `GroupEventNotice`
- `GroupMembershipAction`

### Legacy NIP-72 communities

Layer: typed protocol pack component. Priority: north star. Count: 6.

- `LegacyCommunityCard`
- `LegacyCommunityHeader`
- `LegacyCommunityPostChrome`
- `LegacyApprovalBadge`
- `LegacyApprovalContext`
- `LegacyModerationQueue`

### Zap and value components

Layer: typed protocol pack component. Priority: first showcase. Count: 11.

- `ZapSendSheet`
- `ZapAmountPicker`
- `ZapMessageField`
- `ZapProgress`
- `ZapReceipt`
- `ZapList`
- `ZapTotal`
- `ZapSenderStack`
- `ZapReceiptRow`
- `ReactionReceiptRow`
- `ValueTransferBadge`

### Advanced value components

Layer: typed protocol pack component. Priority: north star. Count: 8.

- `ZapLeaderboard`
- `ZapGoalCard`
- `ZapGoalContributors`
- `ZapGoalContributorRow`
- `NutzapButton`
- `NutzapReceipt`
- `NutzapMintWarning`
- `NutZapReceipt`

### Wallet product recipes

Layer: app-owned product recipe. Priority: north star. Count: 5.

- `NWCConnectionCard`
- `NWCPermissionReview`
- `WalletPaymentConfirmation`
- `ZapGoalProgress`
- `ZapSendClassic`

### Polls

Layer: typed protocol pack component. Priority: north star. Count: 9.

- `PollCard`
- `PollOption`
- `PollVoteButton`
- `PollResults`
- `PollScopeDisclosure`
- `PollParticipantStack`
- `PollVoterSummary`
- `PollClosedBanner`
- `PollVoteAction`

### Badges, labels, and trust

Layer: typed protocol pack component. Priority: north star. Count: 12.

- `BadgeIcon`
- `BadgeAwardCard`
- `BadgeAwardRow`
- `BadgeShelf`
- `BadgeSetCard`
- `LabelChip`
- `LabelStack`
- `LabelDetails`
- `LabeledContentGate`
- `TrustedAssertionBadge`
- `TrustedAssertionScore`
- `AssertionProvenanceSheet`

### Trust product recipes

Layer: app-owned product recipe. Priority: north star. Count: 4.

- `TrustedProviderPicker`
- `TrustLens`
- `ModerationExplanationSheet`
- `ImpersonationWarning`

### Moderation and safety

Layer: linked primitive or headless contract. Priority: first showcase. Count: 7.

- `SensitiveContentGate`
- `MutedContentTombstone`
- `ReportedContentGate`
- `ModerationDecisionBanner`
- `BlockedContentPlaceholder`
- `BlockedEvent`
- `MutedEvent`

### Moderation actions

Layer: typed protocol pack component. Priority: north star. Count: 8.

- `ReportSheet`
- `ReportReasonPicker`
- `ReportReasonChip`
- `ReportSubmissionState`
- `ModerationDecision`
- `ModerationDetailsSheet`
- `ContentWarningComposer`
- `RequestToVanishConfirmation`

### Relay presentation

Layer: source-installed native composition. Priority: first showcase. Count: 17.

- `RelayURLLabel`
- `RelayRoleChip`
- `RelayCardCompact`
- `RelayCardList`
- `RelayCardPortrait`
- `RelayInformationSheet`
- `RelayCapabilityGrid`
- `RelayConnectionState`
- `OutboxRelayStrip`
- `RelayInput`
- `NegentropySyncMinimal`
- `NegentropySyncAnimated`
- `NegentropySyncDetailed`
- `QueryCoverageIndicator`
- `RelayShortfallBanner`
- `RelayListRow`
- `RelayCard`

### Relay and discovery protocol packs

Layer: typed protocol pack component. Priority: north star. Count: 11.

- `InboxRelayStrip`
- `RelayAccessCard`
- `RelayMembershipBadge`
- `RelayJoinRequest`
- `RelayMonitorCard`
- `RelayHealthCard`
- `RelayDiscoveryResult`
- `RelayInfoCard`
- `RelayListCard`
- `OutboxSummary`
- `RelayRecommendationCard`

### Relay settings recipes

Layer: app-owned product recipe. Priority: north star. Count: 4.

- `RelaySettingsScreen`
- `ManualRelayOverride`
- `RelaySelector`
- `RelaySelectorPopover`

### Search and discovery basics

Layer: source-installed native composition. Priority: first showcase. Count: 15.

- `NostrSearchField`
- `SearchScopePicker`
- `SearchSuggestionRow`
- `UnifiedSearchResult`
- `UserSearch`
- `HashtagDiscoveryHeader`
- `ProfileList`
- `EventList`
- `EventGrid`
- `MediaMasonry`
- `TopicRow`
- `NostrListCard`
- `CollectionCover`
- `CuratedFeedCard`
- `UniversalEntityResolver`

### Typed search results

Layer: source-installed native composition. Priority: north star. Count: 6.

- `ArticleSearchResult`
- `PhotoSearchResult`
- `ListingSearchResult`
- `CalendarSearchResult`
- `GroupSearchResult`
- `DiscoveryBento`

### Notification primitives

Layer: linked primitive or headless contract. Priority: first showcase. Count: 4.

- `NotificationRoot`
- `NotificationActorStack`
- `NotificationVerb`
- `NotificationContext`

### Notification compositions

Layer: source-installed native composition. Priority: first showcase. Count: 4.

- `NotificationCompact`
- `NotificationExpanded`
- `NotificationDigest`
- `NotificationUnreadMarker`

### Typed notifications

Layer: source-installed native composition. Priority: north star. Count: 12.

- `FollowNotification`
- `ReactionNotification`
- `ReplyNotification`
- `RepostNotification`
- `MentionNotification`
- `ZapNotification`
- `BadgeAwardNotification`
- `CalendarRSVPNotification`
- `LiveStartedNotification`
- `GroupInviteNotification`
- `CommentNotification`
- `ListingMessageNotification`

### Composer primitives

Layer: linked primitive or headless contract. Priority: first showcase. Count: 4.

- `ComposerRoot`
- `ComposerTextEditor`
- `ComposerAttachmentStrip`
- `ComposerSubmitButton`

### Composer compositions

Layer: source-installed native composition. Priority: first showcase. Count: 19.

- `NoteComposerInline`
- `NoteComposerCard`
- `NoteComposerModal`
- `NoteComposerMinimal`
- `ReplyComposer`
- `QuoteComposer`
- `MentionAutocomplete`
- `HashtagCompletion`
- `EmojiPicker`
- `EmojiCompletion`
- `MediaUploadComposer`
- `WriteIntentProgress`
- `WriteReceiptSheet`
- `UnpublishedEventsButton`
- `UnpublishedEventsPopover`
- `ComposerPreview`
- `PublishProgressSheet`
- `PublishReceiptSheet`
- `SignerPrompt`

### Draft and editor protocol components

Layer: typed protocol pack component. Priority: north star. Count: 3.

- `DraftBadge`
- `DraftCard`
- `DraftCheckpointTimeline`

### Editor and signer recipes

Layer: app-owned product recipe. Priority: north star. Count: 12.

- `DraftBrowser`
- `PollComposer`
- `CalendarEventEditor`
- `SignerConnectionCard`
- `SignRequestReview`
- `AwaitingSignerState`
- `LoginBlock`
- `SignupBlock`
- `AudiencePicker`
- `RelayTargetSummary`
- `DraftRestoreBanner`
- `MuteSheet`

### Git collaboration

Layer: typed protocol pack component. Priority: north star. Count: 10.

- `GitRepositoryCard`
- `GitIssueCard`
- `GitPatchCard`
- `GitPullRequestCard`
- `GitDiffViewer`
- `GitStatusBadge`
- `RepositoryHeader`
- `PatchSeries`
- `CodeReviewComment`
- `RepositoryStatusBadge`

### Additional protocol packs

Layer: typed protocol pack component. Priority: north star. Count: 11.

- `TorrentCard`
- `TorrentFileList`
- `ChessGameCard`
- `ChessBoard`
- `ChessMoveList`
- `NsiteCard`
- `NsiteManifestInspector`
- `SoftwareApplicationCard`
- `ReleaseArtifactCard`
- `EcashMintCard`
- `FedimintCard`

### Legacy and experimental DVMs

Layer: typed protocol pack component. Priority: north star. Count: 9.

- `DVMJobCard`
- `DVMJobStatus`
- `DVMResultSlot`
- `JobRequestCard`
- `JobProgress`
- `JobResultCard`
- `JobProviderCard`
- `JobPaymentStatus`
- `JobFailureCard`

### Creative cross-kind compositions

Layer: source-installed native composition. Priority: first showcase. Count: 12.

- `SmartChannelPreview`
- `ReferenceCarousel`
- `ContextRibbon`
- `NostrShareCard`
- `NostrContextMenu`
- `ProfilePeek`
- `EventPeek`
- `LinkPeek`
- `ReferenceQuickLook`
- `NostrSharePreview`
- `SwipeActionContainer`
- `ReadingProgress`

### North-star cross-kind recipes

Layer: app-owned product recipe. Priority: north star. Count: 11.

- `CreatorConstellation`
- `CreatorStorefront`
- `CommerceConversation`
- `CommunityProvenanceCard`
- `EventJourney`
- `RemixStack`
- `OpenProtocolWorkbench`
- `NostrEntityTransferCard`
- `CrossKindDiscussion`
- `AddressEvolution`
- `SavedCollectionShelf`

### Apple platform recipes

Layer: app-owned product recipe. Priority: north star. Count: 10.

- `EventWidget`
- `ProfileWidget`
- `PhotoWidget`
- `NostrShareComposer`
- `OpenNostrReferenceIntent`
- `NostrSpotlightItemBuilder`
- `WatchEventCard`
- `WatchProfileCard`
- `QRReferenceCard`
- `OpenInClientSheet`

### Gallery shell

Layer: Gallery-only proof instrument. Priority: first showcase. Count: 12.

- `ComponentCatalogue`
- `ComponentFamilySidebar`
- `ComponentDetailPage`
- `LiveExampleStage`
- `VariantStrip`
- `InteractionPlayground`
- `RendererOverridePlayground`
- `AppearanceMatrix`
- `DynamicTypeMatrix`
- `AccessibilityMatrix`
- `PlatformPreviewSwitcher`
- `SeedScenarioPicker`

### Proof journeys

Layer: Gallery-only proof instrument. Priority: first showcase. Count: 18.

- `ProofJourneyList`
- `ProofJourneyHeader`
- `ProofStepTimeline`
- `ProofAssertion`
- `ProofCompletionBadge`
- `ColdCacheJourney`
- `WarmCacheJourney`
- `NoHintAddressJourney`
- `RelayHintJourney`
- `OutboxDiscoveryJourney`
- `AddressReplacementJourney`
- `DemandDeduplicationJourney`
- `DemandWithdrawalJourney`
- `CustomRendererJourney`
- `RecursiveReferenceJourney`
- `HighlightJourney`
- `DisconnectReconnectScenario`
- `CustomKindDemo`

### Routing and acquisition evidence

Layer: Gallery-only proof instrument. Priority: first showcase. Count: 38.

- `LiveDataBadge`
- `FixtureBadge`
- `SeedScenarioCard`
- `ReferenceDecoder`
- `DemandInspector`
- `IndexerBoundaryBanner`
- `OutboxDiscoveryTimeline`
- `RelayContactTimeline`
- `AcquisitionEvidenceTimeline`
- `EventValidationChecklist`
- `ReplaceableWinnerInspector`
- `CacheProvenanceInspector`
- `QueryDedupMeter`
- `ReferenceClaimCounter`
- `HydrationBudgetMeter`
- `RecursionPathViewer`
- `WriteIntentInspector`
- `DecodedReferenceCard`
- `NormalizedTargetCard`
- `CompiledDemandCard`
- `IndexerAllowlistCard`
- `IndexerQueryTimeline`
- `OutboxDiscoveryCard`
- `DiscoveredRelayList`
- `RelayTrafficMatrix`
- `RelayAttemptRow`
- `EOSEEvidenceRow`
- `AcquisitionShortfallCard`
- `SourceAuthorityCard`
- `AccessContextCard`
- `CacheProvenanceCard`
- `CanonicalWinnerCard`
- `CanonicalReplacementTimeline`
- `EventValidationCard`
- `ActiveDemandCounter`
- `SharedDemandBadge`
- `DemandLifecycleTimeline`
- `NetworkBoundaryMonitor`

### Parsing and renderer evidence

Layer: Gallery-only proof instrument. Priority: first showcase. Count: 17.

- `SourceMapOverlay`
- `SourceContentViewer`
- `ContentSyntaxBadge`
- `SemanticDocumentTree`
- `BlockNodeInspector`
- `InlineNodeInspector`
- `ReferenceOccurrenceList`
- `NormalizedTargetList`
- `OccurrenceToTargetMap`
- `ResourceStateInspector`
- `RendererSelectionTrace`
- `RendererPackInspector`
- `RenderPathTree`
- `RecursionBoundaryViewer`
- `RawEventViewer`
- `TypedDecoderViewer`
- `RendererOverrideSwitcher`

### State Lab

Layer: Gallery-only proof instrument. Priority: first showcase. Count: 19.

- `StateLab`
- `ResourceStatePicker`
- `ArtificialLatencyControl`
- `DisconnectedStateControl`
- `MalformedReferenceControl`
- `InvalidEventControl`
- `DeletedEventControl`
- `ExpiredEventControl`
- `ReplacementControl`
- `CycleControl`
- `DepthBudgetControl`
- `HydrationBudgetControl`
- `MediaFailureControl`
- `ReducedMotionControl`
- `IncreasedContrastControl`
- `RightToLeftControl`
- `LargeContentSizeControl`
- `VoiceOverTranscript`
- `AccessibilityInspector`

### Performance and lifecycle proof

Layer: Gallery-only proof instrument. Priority: first showcase. Count: 14.

- `VisibilityClaimOverlay`
- `ViewportHydrationMap`
- `DemandAddRemoveTimeline`
- `DuplicateTargetCounter`
- `RendererIdentityMonitor`
- `FramePacingMeter`
- `SnapshotUpdateCounter`
- `LeafFetchViolationDetector`
- `CacheResetControl`
- `SeedHealthDashboard`
- `SeedRotWarning`
- `ProofReportExporter`
- `PerformanceHUD`
- `PlatformParityChecklist`

### NDK parity style recipes

Layer: app-owned product recipe. Priority: north star. Count: 20.

- `ArticleCardNeonRecipe`
- `UserCardGlassRecipe`
- `UserCardNeonRecipe`
- `ImageCardInstagramRecipe`
- `ThreadViewTwitterRecipe`
- `EventCardClassicRecipe`
- `EventCardBasicRecipe`
- `EventCardBorderedEmphasisRecipe`
- `EventCardMetadataProminentRecipe`
- `EventCardSplitLayoutRecipe`
- `EventCardTimelineStyleRecipe`
- `EventCardAccentBorderRecipe`
- `EventCardHeaderOnlyRecipe`
- `ProgressiveRevealAuthRecipe`
- `LoginCompactRecipe`
- `SignupBlockRecipe`
- `SessionSwitcherCompactRecipe`
- `ZapSendClassicRecipe`
- `RelayBrowserRecipe`
- `HashtagActivityDashboardRecipe`

## Exact NDK styled-component parity ledger

The 84 generated styled entries are all accounted for explicitly:

### Articles: 8/8

- `article-card-inline` -> `ArticleInlineLink`, `ArticleCardInline`
- `article-card-compact` -> `ArticleCardCompact`
- `article-card` -> `ArticleCard`
- `article-card-portrait` -> `ArticleCardPortrait`
- `article-card-neon` -> `ArticleCardNeonRecipe`
- `article-card-hero` -> `ArticleCardHero`
- `article-content-basic` -> `ArticleBody`
- `article-content` -> `ArticleReader`, `HighlightComposer`

### Event chrome and fallback: 7/7

- `event-card` -> `EventRoot`, `EventAuthor`, `EventBody`, `EventActions`, `EventMoreMenu`
- `event-card-basic` -> `EventCardBasicRecipe`
- `event-card-classic` -> `EventCardClassicRecipe`
- `event-card-compact` -> `EventChromeCompactRail`, `CompactEventRow`
- `event-card-inline` -> `EventChromeInline`, `InlineEventEmbed`
- `event-card-fallback` -> `UnknownEventCard`, `OpenWithHandler`
- `fallback-event-basic` -> `UnknownEvent`

### Identity and discovery: 12/12

- `components/avatar-group` -> `AvatarGroup`
- `user-avatar-name` -> `UserAvatarName`
- `user-list-item` -> `UserListItem`
- `user-profile` -> `ProfileSummary`
- `user-profile-hero` -> `ProfileHero`
- `user-card-classic` -> `UserCardClassic`
- `user-card-compact` -> `UserCardCompact`
- `user-card-landscape` -> `UserCardLandscape`
- `user-card-portrait` -> `UserCardPortrait`
- `user-card-glass` -> `UserCardGlassRecipe`
- `user-card-neon` -> `UserCardNeonRecipe`
- `user-search` -> `UserSearch`, `ProfileSearchResult`

### Social actions: 12/12

- `follow-button` -> `FollowButton`
- `follow-button-pill` -> `FollowButtonPill`
- `mute-button` -> `MuteButton`
- `reaction-button` -> `ReactionButton`, `AnimatedReactionToggle`
- `reaction-button-avatars` -> `ReactionButtonAvatars`
- `reaction-button-slack` -> `ReactionButtonSlack`
- `reply-button` -> `ReplyButton`
- `reply-button-avatars` -> `ReplyButtonAvatars`
- `repost-button` -> `RepostButton`, `RepostChoiceMenu`
- `repost-button-avatars` -> `RepostButtonAvatars`
- `zap-button` -> `ZapButton`
- `zap-button-avatars` -> `ZapButtonAvatars`

### Zap flows: 3/3

- `zap-list` -> `ZapList`
- `components/zap-send` -> `ZapSendSheet`, `ZapAmountPicker`, `ZapMessageField`
- `zap-send-classic` -> `ZapSendClassicRecipe`

### Follow packs: 5/5

- `components/follow-pack` -> `FollowPackListItem`
- `follow-pack-compact` -> `FollowPackCompact`
- `follow-pack-hero` -> `FollowPackHero`
- `follow-pack-portrait` -> `FollowPackPortrait`
- `follow-pack-modern` -> `FollowPackModern`

### Hashtags: 4/4

- `components/hashtag` -> `Hashtag`
- `hashtag-modern` -> `HashtagModern`
- `hashtag-card-compact` -> `HashtagCardCompact`
- `hashtag-card-portrait` -> `HashtagCardPortrait`

### Highlights: 5/5

- `highlight-card-inline` -> `HighlightCardInline`
- `highlight-card-compact` -> `HighlightCardCompact`
- `highlight-card-grid` -> `HighlightCardGrid`
- `highlight-card-elegant` -> `HighlightCardElegant`
- `highlight-card-feed` -> `HighlightCardFeed`

### Images: 4/4

- `image-content` -> `ImageContent`
- `image-card-base` -> `ImageCardBase`
- `image-card-hero` -> `ImageCardHero`
- `image-card-instagram` -> `ImageCardInstagram`, `ImageCardInstagramRecipe`

### Links: 2/2

- `link-inline-basic` -> `LinkInlineBasic`
- `link-embed` -> `LinkEmbed`

### Media rendering: 4/4

- `media-basic` -> `MediaBasic`
- `media-bento` -> `MediaBento`
- `media-carousel` -> `MediaCarousel`
- `media-lightbox` -> `MediaLightbox`

### Media upload: 2/2

- `media-upload-button` -> `MediaUploadButton`
- `media-upload-carousel` -> `MediaUploadCarousel`

### Inline mentions: 2/2

- `mention` -> `MentionText`
- `mention-modern` -> `MentionWithAvatar`, `MentionPeek`

### Negentropy: 3/3

- `negentropy-sync-minimal` -> `NegentropySyncMinimal`
- `negentropy-sync-animated` -> `NegentropySyncAnimated`
- `negentropy-sync-detailed` -> `NegentropySyncDetailed`

### Note composition: 1/1

- `note-composer` -> `ComposerRoot`, `ComposerTextEditor`, `ComposerAttachmentStrip`, `ComposerSubmitButton`, `NoteComposerInline`, `NoteComposerCard`, `NoteComposerMinimal`, `NoteComposerModal`

### Notifications: 2/2

- `notification-compact` -> `NotificationCompact`
- `notification-expanded` -> `NotificationExpanded`

### Relays: 4/4

- `relay-card-compact` -> `RelayCardCompact`
- `relay-card-list` -> `RelayCardList`
- `relay-card-portrait` -> `RelayCardPortrait`
- `relay-input` -> `RelayInput`

### Other compositions: 4/4

- `components/content-tab` -> `ContentTabs`
- `components/emoji-picker` -> `EmojiPicker`
- `components/session-switcher` -> `SessionSwitcher`
- `unpublished-events-button-popover` -> `UnpublishedEventsButton`, `UnpublishedEventsPopover`

## NDK primitive, behavior, and block mapping

The 21 primitive families map to content/document rendering, article primitives,
event anatomy, follow-pack primitives, highlight primitives, media upload,
notifications, reactions, relays, user/profile fields, user input, and zap
content in the linked primitive and protocol-pack sections above.

The 24 old builders are not copied as view-owned state. Their capabilities map
to shared semantic parsing, ordinary NMP live queries, typed protocol modules,
app projections, write intents, and receipts. Specifically: event-content and
Markdown extensions map to `NostrContent`; profile/avatar/user/hashtag/search
builders map to supplied resource or app-projected state; thread/notification
builders map to bounded app projections; action builders map to typed emitted
actions plus NMP write intents; relay/negentropy/unpublished-write builders map
to NMP diagnostics and receipts.

The six blocks are classified rather than copied wholesale:

- `login-compact` -> `LoginCompactRecipe`
- `progressive-reveal-auth` -> `ProgressiveRevealAuthRecipe`
- `session-switcher-compact` -> `SessionSwitcherCompactRecipe`
- `blocks/session-switcher` -> `SessionSwitcher`
- `signup-block` -> `SignupBlockRecipe`
- `thread-view-twitter` -> `ThreadViewTwitterRecipe`

## Current protocol caution labels

The main direction favors NIP-99 listings and NIP-29 relay groups. NIP-15 marketplace, NIP-72 communities, NIP-90 DVMs, and NIP-31 unknown-event handling are currently marked unrecommended in the official NIP catalogue, so related components are explicitly legacy or experimental. A permanent UnknownEvent and raw/alt fallback remains required regardless.

## Architecture rejection list

The following are not catalogue components and must fail review:

- An Avatar(pubkey:) that starts its own profile query.
- An ArticleCard(naddr:) that fetches in onAppear.
- A hidden acquisition triggered only by a long-press preview.
- A global mutable renderer singleton or import-driven registration.
- A mandatory application-root component host or service locator.
- A central closed enum of every renderable event kind.
- A UI component that chooses relays, computes canonical winners, or claims global not-found.
- A media view that performs arbitrary HTTP preview work without injected privacy policy.
- An optimistic reaction or follow state presented as confirmed protocol truth.
- A renderer with hardcoded navigation routes, signer ownership, decryption authority, or private keys.
- A Gallery example that silently substitutes a fixture when live data fails.
- A proof panel whose facts originate in Gallery application logic rather than NMP evidence.
