#pragma once

#include "CoreMinimal.h"
#include "AurixVoiceTypes.h"
#include "Interfaces/IHttpRequest.h"

#include "aurix_client.hpp"

/**
 * One `GET /v1/me/regions` followed by parallel per-region RTT probes (sequential samples per
 * region, warm-up discarded), ranked by the native core. HTTP completions arrive on the game
 * thread. Own it through a TSharedPtr: callbacks are bound weakly, so dropping the pointer (or
 * Cancel) stops delivery.
 */
class FAurixRegionDiscovery : public TSharedFromThis<FAurixRegionDiscovery>
{
public:
	DECLARE_DELEGATE_ThreeParams(FOnComplete, bool /*bSuccess*/, const TArray<FAurixRegionEndpoint>& /*Regions*/, const FString& /*Error*/);

	FAurixRegionDiscovery(const FAurixRegionDiscoveryRequest& InRequest, FOnComplete InOnComplete);
	~FAurixRegionDiscovery();

	/** Validates the request and sends the discovery GET. On false nothing was sent and OnComplete will not fire. */
	bool Start(FString& OutError);

	/** Abort in-flight HTTP requests; OnComplete will not fire. */
	void Cancel();

private:
	struct FProbe
	{
		int32 RegionIndex = 0;
		FString Url;
		int32 SamplesLeft = 0;
		bool bWarmupDone = false;
		bool bAnySuccess = false;
		double BestMs = 0.0;
		double StartedAt = 0.0;
		FHttpRequestPtr Request;
	};

	void OnListReceived(FHttpRequestPtr HttpRequest, FHttpResponsePtr Response, bool bConnectedSuccessfully);
	void SendProbe(int32 ProbeIndex);
	void OnProbeReceived(FHttpRequestPtr HttpRequest, FHttpResponsePtr Response, bool bConnectedSuccessfully, int32 ProbeIndex);
	void RankAndFinish();
	void Finish(bool bSuccess, const FString& Error);

	FAurixRegionDiscoveryRequest Request;
	FOnComplete OnComplete;
	FHttpRequestPtr ListRequest;
	TArray<FProbe> Probes;
	int32 PendingProbes = 0;
	bool bDone = false;
	aurix::Regions Regions;
};
