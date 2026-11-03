#pragma once

#include "CoreMinimal.h"

/**
 * Keeps the editor at full frame rate while a bridge client is connected.
 *
 * An agent-driven editor is unfocused by definition, and an unfocused editor
 * with "Use Less CPU when in Background" on ticks a few times a second. Every
 * bridge request waits for a tick, so that setting alone turned a few
 * milliseconds of handler work into hundreds of milliseconds per call, and a
 * queue of agents into a queue of throttled frames.
 *
 * The first connected client turns UEditorPerformanceSettings::
 * bThrottleCPUWhenNotForeground off and the last one to leave puts back
 * whatever the user had. The write is in memory only: nothing is saved, so the
 * user's preference on disk never changes and a crash cannot leave it off.
 *
 * Set UE_MCP_KEEP_THROTTLE=1 (or launch with -MCPKeepThrottle) to leave the
 * setting alone.
 */
namespace MCPBridgeThrottle
{
	/** A client finished its WebSocket upgrade. Any thread. */
	void AddClient();

	/** That client's connection ended. Any thread. Pairs with AddClient. */
	void RemoveClient();

	/**
	 * Drop any queued update and put the user's setting back. Called once the
	 * server has stopped and every connection has ended, from module shutdown
	 * on the game thread; the restore is skipped anywhere else.
	 */
	void Shutdown();
}

/** AddClient for the lifetime of one connection. */
class FMCPThrottleClientScope
{
public:
	FMCPThrottleClientScope() { MCPBridgeThrottle::AddClient(); }
	~FMCPThrottleClientScope() { MCPBridgeThrottle::RemoveClient(); }

	FMCPThrottleClientScope(const FMCPThrottleClientScope&) = delete;
	FMCPThrottleClientScope& operator=(const FMCPThrottleClientScope&) = delete;
};
