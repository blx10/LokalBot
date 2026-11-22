import SwiftUI

struct SpeakerIdentitySettingsControls: View {
    @EnvironmentObject var app: AppState
    @State private var managingProfiles = false

    var body: some View {
        SettingsHelp("When you name a speaker, LokalBot remembers their voice in an encrypted profile on this Mac and names them in later meetings. Automatic names never train profiles, and you can undo one in the transcript.")
            .accessibilityIdentifier("settings.rememberSpeakers")
        if !app.settings.multiSpeakerDiarization {
            SettingsHelp("Turn on “Separate voices by speaker” to collect the voice samples remembering needs.")
        }
        Button("Manage remembered people…") { managingProfiles = true }
            .sheet(isPresented: $managingProfiles) {
                SpeakerVoiceProfileManager(
                    identity: app.speakerIdentity,
                    meetings: app.meetings,
                    storage: app.storage
                )
            }
    }
}

private struct SpeakerVoiceProfileManager: View {
    @ObservedObject var identity: MeetingSpeakerIdentityService
    let meetings: [Meeting]
    let storage: StorageManager
    @Environment(\.dismiss) private var dismiss
    @StateObject private var previewPlayer = MeetingPlayer()
    @State private var people: [SpeakerVoiceProfile] = []
    @State private var error: String?
    @State private var busy = false
    @State private var confirmingClearAll = false
    @State private var playingContributionID: UUID?
    @State private var playingMeetingID: UUID?
    @State private var loadingPreview = false

    private var meetingsByID: [UUID: Meeting] {
        Dictionary(meetings.map { ($0.id, $0) }, uniquingKeysWith: { existing, _ in existing })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Remembered people").font(.scaled(.title2).bold())
            Text("Voice profiles stay on this Mac. Forgetting a person removes their samples and identity links. Existing transcript names remain.")
                .font(.scaled(.callout)).foregroundStyle(.secondary)
            if people.isEmpty {
                Text("No remembered voices yet. Confirm a speaker after a recording with enough clear speech.")
                    .foregroundStyle(.secondary).padding(.vertical)
            }
            ScrollView {
                VStack(spacing: 12) {
                    ForEach(people) { person in
                        let mergeTargets = compatibleTargets(for: person)
                        SpeakerVoiceProfileRow(
                            person: person,
                            mergeTargets: mergeTargets,
                            meetingsByID: meetingsByID,
                            storage: storage,
                            playingContributionID: playingContributionID,
                            onRename: { name in
                                perform { try await identity.renameProfile(person.id, name: name) }
                            },
                            onForget: {
                                stopPreview()
                                perform { try await identity.forgetProfile(person.id) }
                            },
                            onMergeInto: { targetID in
                                stopPreview()
                                perform { try await identity.mergeProfiles(sourceID: person.id, into: targetID) }
                            },
                            onTogglePreview: { contribution in
                                togglePreview(contribution)
                            }
                        )
                    }
                }
            }
            .frame(maxHeight: 360)
            if let error { Text(error).font(.scaled(.caption)).foregroundStyle(.red) }
            HStack {
                Button("Clear all", role: .destructive) { confirmingClearAll = true }
                    .disabled(people.isEmpty)
                    .confirmationDialog("Forget every remembered voice?", isPresented: $confirmingClearAll,
                                        titleVisibility: .visible) {
                        Button("Clear all", role: .destructive) {
                            stopPreview()
                            perform { try await identity.forgetProfile(nil) }
                        }
                        Button("Cancel", role: .cancel) {}
                    }
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(24).frame(width: 560).disabled(busy)
        .task { await reload() }
        .onChange(of: previewPlayer.isPlaying) { _, playing in
            if !playing && !loadingPreview {
                playingContributionID = nil
            }
        }
        .onDisappear { stopPreview() }
    }

    private func compatibleTargets(for source: SpeakerVoiceProfile) -> [SpeakerVoiceProfile] {
        people.filter { candidate in
            guard candidate.id != source.id, candidate.model == source.model else { return false }
            if let s = source.isLocalUser, let t = candidate.isLocalUser, s != t { return false }
            return true
        }
    }

    private func stopPreview() {
        previewPlayer.stop()
        playingContributionID = nil
        playingMeetingID = nil
    }

    private func togglePreview(_ contribution: SpeakerVoiceProfile.Contribution) {
        if previewPlayer.isPlaying && playingContributionID == contribution.decisionID {
            stopPreview()
            return
        }
        guard let meeting = meetingsByID[contribution.meetingID],
              let sampleRange = contribution.samples.first?.range else { return }
        let folder = meeting.folderURL(in: storage)
        guard !MeetingAudioAsset.playbackSources(folder: folder, hasSystemTrack: meeting.hasSystemTrack).isEmpty else { return }
        let start = sampleRange.start
        let desiredEnd = max(sampleRange.end, start + 6)
        if playingMeetingID == meeting.id && previewPlayer.isLoaded {
            playingContributionID = contribution.decisionID
            let end = previewPlayer.duration > 0 ? min(previewPlayer.duration, desiredEnd) : desiredEnd
            previewPlayer.playExcerpt(from: start, to: end)
            return
        }
        loadingPreview = true
        playingContributionID = contribution.decisionID
        playingMeetingID = meeting.id
        Task {
            defer { loadingPreview = false }
            await previewPlayer.loadInBackground(folder: folder, hasSystemTrack: meeting.hasSystemTrack)
            guard previewPlayer.isLoaded, playingContributionID == contribution.decisionID else {
                if !previewPlayer.isPlaying {
                    playingContributionID = nil
                    playingMeetingID = nil
                }
                return
            }
            let end = previewPlayer.duration > 0 ? min(previewPlayer.duration, desiredEnd) : desiredEnd
            previewPlayer.playExcerpt(from: start, to: end)
        }
    }

    private func reload() async {
        do { people = try await identity.profiles() } catch { self.error = "Could not read remembered people: \(error.localizedDescription)" }
    }
    private func perform(_ action: @escaping @MainActor () async throws -> Void) {
        busy = true
        Task {
            defer { busy = false }
            do { try await action(); error = nil; await reload() } catch { self.error = "Change pending: \(error.localizedDescription)" }
        }
    }
}

private struct SpeakerVoiceProfileRow: View {
    let person: SpeakerVoiceProfile
    let mergeTargets: [SpeakerVoiceProfile]
    let meetingsByID: [UUID: Meeting]
    let storage: StorageManager
    let playingContributionID: UUID?
    let onRename: (String) -> Void
    let onForget: () -> Void
    let onMergeInto: (UUID) -> Void
    let onTogglePreview: (SpeakerVoiceProfile.Contribution) -> Void
    @State private var name: String
    @State private var showingSources = false

    init(
        person: SpeakerVoiceProfile,
        mergeTargets: [SpeakerVoiceProfile],
        meetingsByID: [UUID: Meeting],
        storage: StorageManager,
        playingContributionID: UUID?,
        onRename: @escaping (String) -> Void,
        onForget: @escaping () -> Void,
        onMergeInto: @escaping (UUID) -> Void,
        onTogglePreview: @escaping (SpeakerVoiceProfile.Contribution) -> Void
    ) {
        self.person = person
        self.mergeTargets = mergeTargets
        self.meetingsByID = meetingsByID
        self.storage = storage
        self.playingContributionID = playingContributionID
        self.onRename = onRename
        self.onForget = onForget
        self.onMergeInto = onMergeInto
        self.onTogglePreview = onTogglePreview
        _name = State(initialValue: person.name)
    }

    private var sortedContributions: [SpeakerVoiceProfile.Contribution] {
        person.contributions.sorted { $0.confirmedAt > $1.confirmedAt }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                TextField("Person's name", text: $name).textFieldStyle(.roundedBorder)
                Button("Rename") { onRename(name) }.disabled(name == person.name || ParticipantObservation.safeName(name) == nil)
                if !mergeTargets.isEmpty {
                    Menu("Merge into…") {
                        ForEach(mergeTargets) { target in
                            Button(target.name) { onMergeInto(target.id) }
                        }
                    }
                }
                Button("Forget", role: .destructive, action: onForget)
            }
            Text("\(person.contributions.count) confirmed recording(s) · Profile \(person.id.uuidString.prefix(6))")
                .font(.scaled(.caption)).foregroundStyle(.secondary)
            if !person.contributions.isEmpty {
                DisclosureGroup("Learned from \(person.contributions.count) meeting(s)", isExpanded: $showingSources) {
                    VStack(alignment: .leading, spacing: 6) {
                        ForEach(sortedContributions, id: \.decisionID) { contribution in
                            contributionRow(contribution)
                        }
                    }
                    .padding(.top, 4)
                }
                .font(.scaled(.caption))
            }
        }
        .padding(10)
        .lbGroupedSurface()
    }

    @ViewBuilder
    private func contributionRow(_ contribution: SpeakerVoiceProfile.Contribution) -> some View {
        let meeting = meetingsByID[contribution.meetingID]
        let title = meeting?.displayTitle ?? contribution.meetingTitle ?? "Meeting"
        let started = meeting?.startedAt ?? contribution.meetingStartedAt ?? contribution.confirmedAt
        let hasAudio = meeting.map {
            !MeetingAudioAsset.playbackSources(folder: $0.folderURL(in: storage), hasSystemTrack: $0.hasSystemTrack).isEmpty
        } ?? false
        let canPreview = hasAudio && contribution.samples.first?.range != nil
        let isPlaying = playingContributionID == contribution.decisionID
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.scaled(.caption).weight(.medium))
                    .lineLimit(1)
                Text(started.formatted(date: .abbreviated, time: .shortened))
                    .font(.scaled(.caption2))
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if canPreview {
                Button(isPlaying ? "Stop" : "Play") {
                    onTogglePreview(contribution)
                }
                .buttonStyle(.bordered)
                .controlSize(.mini)
            } else {
                Text("Recording deleted")
                    .font(.scaled(.caption2))
                    .foregroundStyle(.secondary)
            }
        }
    }
}

struct SpeakerIdentityReview: View {
    let speaker: String
    let state: MeetingSpeakerIdentityState?
    @Binding var name: String
    @Binding var profileID: UUID?
    let onPlay: (Double) -> Void
    let onAction: (SpeakerAliasDecision.Action, String?, UUID?) -> Void
    let onDeleteEvidence: () -> Void
    var canConfirmIdentity = false
    var microphoneIsUser = false
    var showsSuggestions = true
    private var assignment: SpeakerIdentityAssignment? { state?.assignments.first { $0.label == speaker } }

    private var identityDescription: String {
        if let isUser = assignment?.isLocalUser { return isUser ? "Confirmed as you" : "Confirmed as someone else" }
        return microphoneIsUser ? "Attributed to you from your microphone" : "Confirm who is speaking"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if canConfirmIdentity {
                Text(identityDescription)
                    .font(.scaled(.subheadline).weight(.semibold))
                HStack {
                    if let time = assignment?.anchors.first?.start { Button("Play Speech") { onPlay(time) } }
                    Button("This Is Me") { onAction(.confirmUser, name, profileID) }
                    Button("Someone Else") { onAction(.confirmOther, name, profileID) }
                }
            } else {
                Text("This audio mixes voices, so it can't be confirmed as one person. You can still name it.")
                    .font(.scaled(.caption)).foregroundStyle(.secondary)
            }
            if let match = assignment?.match, assignment?.origin.isProtected == false {
                Text("Automatically identified").font(.scaled(.subheadline).weight(.semibold))
                Text(match.explanation).font(.scaled(.caption)).foregroundStyle(.secondary)
                HStack {
                    if let time = match.evidence.first?.start { Button("Play supporting speech") { onPlay(time) } }
                    Button("Undo") { onAction(.undo, assignment?.name, nil) }
                }
            }
            if assignment?.automaticDisabled == true {
                Text("Automatic naming is paused for this speaker.").font(.scaled(.caption)).foregroundStyle(.secondary)
                Button("Resume automatic identification") { onAction(.resume, nil, nil) }
            }
            if showsSuggestions, let candidates = state?.suggestions[speaker], !candidates.isEmpty, assignment?.origin.isProtected != true {
                Text("Suggested Names").font(.scaled(.headline))
                ForEach(candidates) { candidate in
                    VStack(alignment: .leading, spacing: 4) {
                        HStack {
                            Button(candidate.name) { name = candidate.name; profileID = candidate.profileID }
                            Spacer()
                            if let time = candidate.evidence.first?.start { Button("Play") { onPlay(time) } }
                            Button("Dismiss") { onAction(.dismiss, candidate.name, nil) }
                        }
                        Text(candidate.explanation).font(.scaled(.caption)).foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(12)
                    .lbGroupedSurface()
                }
            }
            if let previous = state?.assignments.filter({ $0.label.hasPrefix("unresolved-") && $0.name != nil }), !previous.isEmpty {
                Text("Previously named voices").font(.scaled(.subheadline).weight(.semibold))
                Text("Speaker separation changed. Play the saved speech before choosing a previous name for this speaker.")
                    .font(.scaled(.caption)).foregroundStyle(.secondary)
                ForEach(previous.prefix(6)) { item in
                    HStack {
                        Button(item.name ?? "Unnamed") { name = item.name ?? ""; profileID = nil }
                        if let time = item.anchors.first?.start { Button("Play saved speech") { onPlay(time) } }
                    }
                }
            }
            if state?.revision ?? 0 > 0 {
                Button("Delete meeting speaker evidence", role: .destructive, action: onDeleteEvidence)
                    .font(.scaled(.caption))
                Text("Keeps your saved speaker names and remembered voice profiles.")
                    .font(.scaled(.caption)).foregroundStyle(.secondary)
            }
        }
    }
}
