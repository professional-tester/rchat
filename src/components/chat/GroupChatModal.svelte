<script lang="ts">
  import { open } from "@tauri-apps/plugin-dialog";
  import { fade } from "svelte/transition";
  import { getChatKind, isTemporaryGroupChatId } from "$lib/chatKind";
  import { displayNameFromChatId } from "$lib/chatIdentity";
  import { api } from "$lib/tauri/api";

  type GroupCreatePayload = {
    name: string;
    imagePath: string | null;
    membersCanInvite: boolean;
    invitePeerIds: string[];
  };

  let {
    show = false,
    peers = [] as string[],
    peerAliases = {} as Record<string, string | null>,
    chatNames = {} as Record<string, string>,
    groupChats = {} as Record<string, boolean>,
    connectedChatIds = [] as string[],
    addedPeerId = null as string | null,
    waitingForNewPerson = false,
    onclose = () => {},
    oncreate = async (_payload: GroupCreatePayload) => {},
    ontempjoin = async (_chatId: string, _name: string) => {},
    onnewperson = () => {},
    onconsumeaddedpeer = () => {},
  } = $props();

  let mode = $state<"wizard" | "temporary">("wizard");
  let step = $state<"settings" | "invite">("settings");
  let groupName = $state("");
  let groupImagePath = $state<string | null>(null);
  let groupImagePreview = $state<string | null>(null);
  let membersCanInvite = $state(false);
  let selectedPeerIds = $state<string[]>([]);
  let tempMode = $state<"create" | "redeem">("create");
  let tempGroupName = $state("");
  let tempInviteLink = $state("");
  let tempRedeemLink = $state("");
  let tempInviteRemaining = $state(0);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let countdownTimer: ReturnType<typeof setInterval> | null = null;

  let availablePeers = $derived(
    peers.filter((peerId) => {
      const kind = getChatKind(peerId);
      return kind === "dm" && !groupChats[peerId];
    })
  );

  $effect(() => {
    if (!show) return;
    if (!countdownTimer) {
      countdownTimer = setInterval(() => {
        if (tempInviteRemaining > 0) {
          tempInviteRemaining -= 1;
        }
      }, 1000);
    }
    return () => {
      if (countdownTimer) {
        clearInterval(countdownTimer);
        countdownTimer = null;
      }
    };
  });

  $effect(() => {
    if (!show || !addedPeerId) return;
    if (!selectedPeerIds.includes(addedPeerId)) {
      selectedPeerIds = [...selectedPeerIds, addedPeerId];
    }
    mode = "wizard";
    step = "invite";
    error = null;
    onconsumeaddedpeer();
  });

  function reset() {
    mode = "wizard";
    step = "settings";
    groupName = "";
    groupImagePath = null;
    groupImagePreview = null;
    membersCanInvite = false;
    selectedPeerIds = [];
    tempMode = "create";
    tempGroupName = "";
    tempInviteLink = "";
    tempRedeemLink = "";
    tempInviteRemaining = 0;
    error = null;
    busy = false;
  }

  function close() {
    reset();
    onclose();
  }

  function peerLabel(peerId: string) {
    return chatNames[peerId] || peerAliases[peerId] || displayNameFromChatId(peerId);
  }

  function peerSubtitle(peerId: string) {
    return connectedChatIds.includes(peerId) ? "connected" : "known peer";
  }

  function togglePeer(peerId: string) {
    selectedPeerIds = selectedPeerIds.includes(peerId)
      ? selectedPeerIds.filter((id) => id !== peerId)
      : [...selectedPeerIds, peerId];
  }

  async function pickGroupImage() {
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [
          {
            name: "Images",
            extensions: ["png", "jpg", "jpeg", "gif", "webp"],
          },
        ],
      });
      if (typeof selected !== "string") return;
      groupImagePath = selected;
      groupImagePreview = await api.getImageFromPath(selected);
      error = null;
    } catch (e: any) {
      error = e?.toString?.() || "Failed to load group image";
    }
  }

  function clearGroupImage() {
    groupImagePath = null;
    groupImagePreview = null;
  }

  function continueToInvite() {
    if (!groupName.trim()) {
      error = "Group name is required";
      return;
    }
    error = null;
    step = "invite";
  }

  async function submitWizard(invitePeerIds = selectedPeerIds) {
    if (busy) return;
    const name = groupName.trim();
    if (!name) {
      error = "Group name is required";
      step = "settings";
      return;
    }
    busy = true;
    error = null;
    try {
      await oncreate({
        name,
        imagePath: groupImagePath,
        membersCanInvite,
        invitePeerIds,
      });
      reset();
    } catch (e: any) {
      error = e?.toString?.() || "Failed to create group";
    } finally {
      busy = false;
    }
  }

  async function refreshTempInvite() {
    try {
      const active = await api.getActiveTemporaryInvite();
      if (active && active.payload.kind === "group") {
        tempInviteLink = active.deep_link;
        tempInviteRemaining = active.remaining_seconds;
      } else {
        tempInviteLink = "";
        tempInviteRemaining = 0;
      }
    } catch {
      tempInviteLink = "";
      tempInviteRemaining = 0;
    }
  }

  async function submitTempCreate() {
    if (busy) return;
    busy = true;
    error = null;
    try {
      const result = await api.createTemporaryInvite("group", tempGroupName.trim() || null);
      tempInviteLink = result.deep_link;
      tempInviteRemaining = result.remaining_seconds;
      tempMode = "redeem";
    } catch (e: any) {
      error = e?.toString?.() || "Failed to create temporary group invite";
    } finally {
      busy = false;
    }
  }

  async function submitTempRedeem() {
    if (busy) return;
    const link = tempRedeemLink.trim();
    if (!link) {
      error = "Paste temporary invite link";
      return;
    }
    busy = true;
    error = null;
    try {
      const result = await api.redeemTemporaryInvite(link);
      if (result.kind !== "group" || !isTemporaryGroupChatId(result.chat_id)) {
        throw new Error("This temporary invite is not a group invite");
      }
      await ontempjoin(result.chat_id, result.name);
      reset();
    } catch (e: any) {
      error = e?.toString?.() || "Failed to redeem temporary group invite";
    } finally {
      busy = false;
    }
  }

  async function copyTempLink() {
    if (!tempInviteLink) return;
    try {
      await navigator.clipboard.writeText(tempInviteLink);
    } catch {
      // Ignore copy errors.
    }
  }

  async function cancelTempInvite() {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await api.cancelTemporaryInvite();
      tempInviteLink = "";
      tempInviteRemaining = 0;
    } catch (e: any) {
      error = e?.toString?.() || "Failed to cancel temporary invite";
    } finally {
      busy = false;
    }
  }
</script>

{#if show}
  <div
    class="fixed inset-0 bg-black/70 z-50 flex items-center justify-center p-4 animate-fade-in-up"
    transition:fade={{ duration: 150 }}
    role="button"
    tabindex="0"
    onclick={(e) => {
      if (e.target === e.currentTarget) close();
    }}
    onkeydown={(e) => {
      if (e.key === "Escape") close();
    }}
  >
    <div
      class="bg-theme-base-900 border border-theme-base-700 rounded-2xl w-full max-w-3xl shadow-2xl overflow-hidden"
    >
      <div class="flex items-center justify-between border-b border-theme-base-800 px-6 py-4">
        <div>
          <h3 class="text-xl font-bold text-theme-base-100">New Group</h3>
          <p class="text-xs text-theme-base-500">
            Create an invite-gated group. People join only after accepting an invite.
          </p>
        </div>
        <div class="flex gap-2">
          <button
            class={`px-3 py-2 rounded-lg text-sm transition-colors ${mode === "wizard" ? "bg-theme-primary-600 text-white" : "bg-theme-base-800 text-theme-base-300 hover:text-white"}`}
            onclick={() => {
              mode = "wizard";
              error = null;
            }}
          >
            New Group
          </button>
          <button
            class={`px-3 py-2 rounded-lg text-sm transition-colors ${mode === "temporary" ? "bg-theme-warning-600 text-white" : "bg-theme-base-800 text-theme-base-300 hover:text-white"}`}
            onclick={async () => {
              mode = "temporary";
              error = null;
              await refreshTempInvite();
            }}
          >
            Temporary Group
          </button>
        </div>
      </div>

      <div class="p-6 space-y-5">
        {#if mode === "wizard"}
          <div class="flex items-center gap-3 text-xs uppercase tracking-wide text-theme-base-500">
            <span class={step === "settings" ? "text-theme-primary-300" : ""}>
              1. Group Settings
            </span>
            <span>/</span>
            <span class={step === "invite" ? "text-theme-primary-300" : ""}>
              2. Invite People
            </span>
          </div>

          {#if step === "settings"}
            <div class="grid gap-5 md:grid-cols-[1fr_220px]">
              <div class="space-y-4">
                <div>
                  <label class="block text-xs text-theme-base-400 uppercase tracking-wide" for="group-name">
                    Group Name
                  </label>
                  <input
                    id="group-name"
                    type="text"
                    bind:value={groupName}
                    placeholder="Project friends"
                    class="mt-2 w-full rounded-lg bg-theme-base-800 border border-theme-base-700 px-3 py-2 text-sm text-theme-base-100 focus:outline-none focus:border-theme-primary-500"
                    onkeydown={(e) => e.key === "Enter" && continueToInvite()}
                  />
                </div>

                <label class="flex items-start gap-3 rounded-lg border border-theme-base-800 bg-theme-base-950/60 p-3">
                  <input
                    type="checkbox"
                    bind:checked={membersCanInvite}
                    class="mt-1"
                  />
                  <span>
                    <span class="block text-sm text-theme-base-100">Members can invite people</span>
                    <span class="block text-xs text-theme-base-500">
                      Off by default. The founder can always invite.
                    </span>
                  </span>
                </label>
              </div>

              <div class="space-y-3">
                <p class="text-xs text-theme-base-400 uppercase tracking-wide">Group Image</p>
                <button
                  class="aspect-square w-full rounded-xl border border-dashed border-theme-base-700 bg-theme-base-950 overflow-hidden flex items-center justify-center text-sm text-theme-base-500 hover:border-theme-primary-500 hover:text-theme-base-200"
                  onclick={pickGroupImage}
                  type="button"
                >
                  {#if groupImagePreview}
                    <img src={groupImagePreview} alt="Selected group" class="h-full w-full object-cover" />
                  {:else}
                    Select Image
                  {/if}
                </button>
                {#if groupImagePath}
                  <div class="flex items-center gap-2">
                    <button
                      type="button"
                      class="text-xs text-theme-base-400 hover:text-white"
                      onclick={pickGroupImage}
                    >
                      Change
                    </button>
                    <button
                      type="button"
                      class="text-xs text-theme-error-300 hover:text-theme-error-200"
                      onclick={clearGroupImage}
                    >
                      Remove
                    </button>
                  </div>
                {/if}
              </div>
            </div>
          {:else}
            <div class="space-y-4">
              <div class="flex items-center justify-between gap-3">
                <div>
                  <h4 class="text-sm font-semibold text-theme-base-100">Invite People</h4>
                  <p class="text-xs text-theme-base-500">
                    Selected peers receive group invitations. They become members after accepting.
                  </p>
                </div>
                <button
                  type="button"
                  class="px-3 py-2 rounded-lg bg-theme-base-800 text-sm text-theme-base-200 hover:bg-theme-base-700"
                  onclick={() => onnewperson()}
                >
                  New Person
                </button>
              </div>

              {#if waitingForNewPerson}
                <p class="rounded-lg border border-theme-warning-700 bg-theme-warning-950/30 px-3 py-2 text-xs text-theme-warning-200">
                  Waiting for peer to connect before they can be invited.
                </p>
              {/if}

              <div class="max-h-64 overflow-y-auto rounded-xl border border-theme-base-800 bg-theme-base-950/60">
                {#if availablePeers.length === 0}
                  <p class="p-4 text-sm text-theme-base-500">
                    No existing direct peers yet. Use New Person or skip for now.
                  </p>
                {:else}
                  {#each availablePeers as peerId}
                    <button
                      type="button"
                      class="w-full flex items-center justify-between gap-3 px-4 py-3 text-left border-b border-theme-base-900 last:border-b-0 hover:bg-theme-base-800/70"
                      onclick={() => togglePeer(peerId)}
                    >
                      <span>
                        <span class="block text-sm text-theme-base-100">{peerLabel(peerId)}</span>
                        <span class="block text-xs text-theme-base-500">{peerSubtitle(peerId)}</span>
                      </span>
                      <span
                        class={`h-5 w-5 rounded border flex items-center justify-center text-xs ${selectedPeerIds.includes(peerId) ? "bg-theme-primary-500 border-theme-primary-400 text-white" : "border-theme-base-600 text-theme-base-600"}`}
                      >
                        {selectedPeerIds.includes(peerId) ? "x" : ""}
                      </span>
                    </button>
                  {/each}
                {/if}
              </div>
            </div>
          {/if}
        {:else}
          <div class="space-y-4">
            <div class="flex gap-2">
              <button
                class={`px-3 py-2 rounded-lg text-sm transition-colors ${tempMode === "create" ? "bg-theme-base-700 text-white" : "bg-theme-base-800 text-theme-base-300 hover:text-white"}`}
                onclick={() => {
                  tempMode = "create";
                  error = null;
                }}
              >
                Create
              </button>
              <button
                class={`px-3 py-2 rounded-lg text-sm transition-colors ${tempMode === "redeem" ? "bg-theme-base-700 text-white" : "bg-theme-base-800 text-theme-base-300 hover:text-white"}`}
                onclick={async () => {
                  tempMode = "redeem";
                  error = null;
                  await refreshTempInvite();
                }}
              >
                Redeem
              </button>
            </div>

            {#if tempMode === "create"}
              <div class="space-y-3">
                <label class="block text-xs text-theme-base-400 uppercase tracking-wide" for="temp-group-name">
                  Temporary Group Name (optional)
                </label>
                <input
                  id="temp-group-name"
                  type="text"
                  bind:value={tempGroupName}
                  placeholder="Temporary Squad"
                  class="w-full rounded-lg bg-theme-base-800 border border-theme-base-700 px-3 py-2 text-sm text-theme-base-100 focus:outline-none focus:border-theme-warning-500"
                />
                {#if tempInviteLink}
                  <div class="p-3 rounded-lg border border-theme-base-700 bg-theme-base-950 space-y-2">
                    <p class="text-xs text-theme-base-500">
                      Active invite expires in
                      <span class="text-theme-warning-400 font-semibold">{tempInviteRemaining}s</span>
                    </p>
                    <code class="block text-xs text-theme-warning-300 break-all">
                      {tempInviteLink}
                    </code>
                    <div class="flex gap-2">
                      <button
                        onclick={copyTempLink}
                        class="px-3 py-1.5 text-xs rounded-md bg-theme-base-800 hover:bg-theme-base-700 text-theme-base-200"
                      >
                        Copy Link
                      </button>
                      <button
                        onclick={cancelTempInvite}
                        class="px-3 py-1.5 text-xs rounded-md bg-theme-base-800 hover:bg-theme-base-700 text-theme-error-300"
                      >
                        Cancel Invite
                      </button>
                    </div>
                  </div>
                {/if}
              </div>
            {:else}
              <div class="space-y-3">
                <label class="block text-xs text-theme-base-400 uppercase tracking-wide" for="temp-group-link">
                  Temporary Invite Link
                </label>
                <input
                  id="temp-group-link"
                  type="text"
                  bind:value={tempRedeemLink}
                  placeholder="rchat://temp/..."
                  class="w-full rounded-lg bg-theme-base-800 border border-theme-base-700 px-3 py-2 text-sm text-theme-base-100 focus:outline-none focus:border-theme-warning-500"
                  onkeydown={(e) => e.key === "Enter" && submitTempRedeem()}
                />
              </div>
            {/if}
          </div>
        {/if}

        {#if error}
          <p class="text-sm text-theme-error-400">{error}</p>
        {/if}
      </div>

      <div class="flex justify-end gap-2 border-t border-theme-base-800 px-6 py-4">
        <button
          onclick={close}
          class="px-4 py-2 text-sm text-theme-base-400 hover:text-white transition-colors"
        >
          Close
        </button>
        {#if mode === "wizard" && step === "settings"}
          <button
            onclick={continueToInvite}
            class="px-4 py-2 text-sm rounded-lg bg-theme-primary-600 hover:bg-theme-primary-500 text-white disabled:opacity-60"
            disabled={!groupName.trim()}
          >
            Continue
          </button>
        {:else if mode === "wizard"}
          <button
            onclick={() => (step = "settings")}
            class="px-4 py-2 text-sm rounded-lg bg-theme-base-800 hover:bg-theme-base-700 text-theme-base-200"
          >
            Back
          </button>
          <button
            onclick={() => submitWizard([])}
            class="px-4 py-2 text-sm rounded-lg bg-theme-base-800 hover:bg-theme-base-700 text-theme-base-200 disabled:opacity-60"
            disabled={busy}
          >
            Skip
          </button>
          <button
            onclick={() => submitWizard()}
            class="px-4 py-2 text-sm rounded-lg bg-theme-primary-600 hover:bg-theme-primary-500 text-white disabled:opacity-60"
            disabled={busy}
          >
            {busy ? "Creating..." : "Invite People"}
          </button>
        {:else}
          <button
            onclick={tempMode === "create" ? submitTempCreate : submitTempRedeem}
            class="px-4 py-2 text-sm rounded-lg bg-theme-warning-600 hover:bg-theme-warning-500 text-white disabled:opacity-60"
            disabled={busy}
          >
            {#if busy}
              Working...
            {:else if tempMode === "create"}
              Create Temp Invite
            {:else}
              Redeem Temp Invite
            {/if}
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}
