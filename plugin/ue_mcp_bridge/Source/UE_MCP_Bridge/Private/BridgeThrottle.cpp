#include "BridgeThrottle.h"
#include "UE_MCP_BridgeModule.h"
#include "Containers/Ticker.h"
#include "Editor.h"
#include "Editor/EditorPerformanceSettings.h"
#include "HAL/CriticalSection.h"
#include "HAL/PlatformMisc.h"
#include "HAL/ThreadSafeBool.h"
#include "HAL/ThreadSafeCounter.h"
#include "Misc/CommandLine.h"
#include "Misc/Parse.h"
#include "Misc/ScopeLock.h"
#include "UObject/UObjectBase.h"

namespace
{
	// Connected clients, counted from any connection thread.
	FThreadSafeCounter GClientCount;

	// Set from the moment a reconcile is queued until it starts. A reconcile
	// reads the count when it runs rather than when it was queued, so one queued
	// update covers every connect and disconnect that happens before it fires.
	FThreadSafeBool GReconcileQueued;

	// Guards the queued ticker's handle and the shut-down flag. The ticker
	// callback never takes it, so holding it across AddTicker cannot invert
	// against whatever lock the ticker holds while it runs callbacks.
	FCriticalSection GQueueMutex;
	FTSTicker::FDelegateHandle GQueuedTicker;
	bool GShutDown = false;

	// Game thread only: whether this module is holding the setting off, and
	// what the user had before it did.
	bool GHoldingThrottleOff = false;
	bool GUserThrottleSetting = false;

	bool IsSuspendDisabled()
	{
		static const bool bDisabled = []
		{
			if (FParse::Param(FCommandLine::Get(), TEXT("MCPKeepThrottle")))
			{
				return true;
			}
			const FString EnvValue = FPlatformMisc::GetEnvironmentVariable(TEXT("UE_MCP_KEEP_THROTTLE"));
			return EnvValue == TEXT("1") || EnvValue.Equals(TEXT("true"), ESearchCase::IgnoreCase)
				|| EnvValue.Equals(TEXT("yes"), ESearchCase::IgnoreCase);
		}();
		return bDisabled;
	}

	void RestoreUserSetting(UEditorPerformanceSettings& Settings)
	{
		GHoldingThrottleOff = false;
		// Only put back what this module took away. If the setting is already on,
		// someone turned it on while it was held, and that is the newer answer.
		if (GUserThrottleSetting && !Settings.bThrottleCPUWhenNotForeground)
		{
			Settings.bThrottleCPUWhenNotForeground = true;
			UE_LOG(LogMCPBridge, Log, TEXT("[UE-MCP] Last bridge client disconnected; background CPU throttling restored."));
		}
	}

	/**
	 * Bring the setting in line with the current client count. Game thread.
	 * False while the editor is still starting, which keeps the ticker alive
	 * until it is not.
	 */
	bool Reconcile()
	{
		// The module loads at PostConfigInit and the socket opens with it, so a
		// client that keeps retrying can connect long before the editor engine
		// exists. The settings object is not something to touch that early.
		if (!GEditor)
		{
			return false;
		}

		// Cleared before the count is read, so a connect landing after the read
		// queues a fresh reconcile instead of being lost.
		GReconcileQueued = false;
		const int32 Clients = GClientCount.GetValue();

		UEditorPerformanceSettings* Settings = GetMutableDefault<UEditorPerformanceSettings>();
		if (!Settings)
		{
			return true;
		}

		if (Clients > 0)
		{
			if (!GHoldingThrottleOff)
			{
				GUserThrottleSetting = Settings->bThrottleCPUWhenNotForeground != 0;
				GHoldingThrottleOff = true;
				if (GUserThrottleSetting)
				{
					UE_LOG(LogMCPBridge, Log,
						TEXT("[UE-MCP] Bridge client connected; background CPU throttling suspended until the last one leaves (in memory only, nothing is saved). Set UE_MCP_KEEP_THROTTLE=1 to keep it."));
				}
			}
			else if (Settings->bThrottleCPUWhenNotForeground)
			{
				// Turned back on while held. That is a request to have it on, so it
				// is what the restore puts back.
				GUserThrottleSetting = true;
			}
			// No SaveConfig and no PostEditChange: the editor reads this flag every
			// frame, and a write that never reaches the config cache is one that
			// cannot outlive the session.
			Settings->bThrottleCPUWhenNotForeground = false;
		}
		else if (GHoldingThrottleOff)
		{
			RestoreUserSetting(*Settings);
		}
		return true;
	}

	void QueueReconcile()
	{
		if (GReconcileQueued.AtomicSet(true))
		{
			return; // one is already waiting, and it will read the new count
		}

		FScopeLock Lock(&GQueueMutex);
		if (GShutDown)
		{
			return;
		}
		// The settings object is a UObject, so the write happens on the game
		// thread, through the same core ticker the request executor uses.
		GQueuedTicker = FTSTicker::GetCoreTicker().AddTicker(
			FTickerDelegate::CreateLambda([](float) -> bool
			{
				return !Reconcile(); // one-shot once it has run
			}));
	}
}

void MCPBridgeThrottle::AddClient()
{
	if (IsSuspendDisabled())
	{
		return;
	}
	GClientCount.Increment();
	QueueReconcile();
}

void MCPBridgeThrottle::RemoveClient()
{
	if (IsSuspendDisabled())
	{
		return;
	}
	GClientCount.Decrement();
	QueueReconcile();
}

void MCPBridgeThrottle::Shutdown()
{
	// A queued callback lives in this module's code, so it must not outlive it.
	{
		FScopeLock Lock(&GQueueMutex);
		GShutDown = true;
		if (GQueuedTicker.IsValid())
		{
			FTSTicker::GetCoreTicker().RemoveTicker(GQueuedTicker);
			GQueuedTicker.Reset();
		}
	}

	// In memory only, so there is nothing to undo once the object system is gone.
	if (!IsInGameThread() || !GHoldingThrottleOff || !UObjectInitialized())
	{
		return;
	}
	if (UEditorPerformanceSettings* Settings = GetMutableDefault<UEditorPerformanceSettings>())
	{
		RestoreUserSetting(*Settings);
	}
}
