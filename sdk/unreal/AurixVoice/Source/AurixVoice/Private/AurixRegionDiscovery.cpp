#include "AurixRegionDiscovery.h"

#include "AurixNativeConversions.h"
#include "AurixVoiceLog.h"
#include "HttpModule.h"
#include "Interfaces/IHttpResponse.h"

namespace
{
constexpr float MinListTimeoutSeconds = 10.f;

bool IsReachable(const FHttpResponsePtr& Response, bool bConnectedSuccessfully)
{
	// Any HTTP answer proves the node is reachable; only transport failures count against it.
	return bConnectedSuccessfully && Response.IsValid() && Response->GetResponseCode() > 0;
}

FAurixRegionEndpoint ToEndpoint(const AurixRegionEndpoint& R)
{
	FAurixRegionEndpoint Out;
	Out.Region = FromUtf8(R.region);
	Out.NodeId = ToGuid(R.node_id);
	Out.WsUrl = FromUtf8(R.ws_url);
	Out.ProbeUrl = FromUtf8(R.probe_url);
	Out.bHasLocation = R.has_location;
	Out.Latitude = R.latitude;
	Out.Longitude = R.longitude;
	Out.bHasDistance = R.has_distance;
	Out.DistanceKm = static_cast<float>(R.distance_km);
	Out.Nodes = static_cast<int32>(R.nodes);
	Out.LoadFactor = R.load_factor;
	Out.bHasRtt = R.has_rtt;
	Out.RttMs = static_cast<float>(R.rtt_ms);
	Out.bProbeFailed = R.probe_failed;
	return Out;
}
} // namespace

FAurixRegionDiscovery::FAurixRegionDiscovery(const FAurixRegionDiscoveryRequest& InRequest, FOnComplete InOnComplete)
	: Request(InRequest)
	, OnComplete(MoveTemp(InOnComplete))
{
}

FAurixRegionDiscovery::~FAurixRegionDiscovery()
{
	Cancel();
}

bool FAurixRegionDiscovery::Start(FString& OutError)
{
	if (Request.ApiUrl.TrimStartAndEnd().IsEmpty())
	{
		OutError = TEXT("ApiUrl is empty");
		return false;
	}
	if (Request.Token.IsEmpty())
	{
		OutError = TEXT("Token is empty");
		return false;
	}
	if (Request.bProbe && Request.ProbeSamples < 1)
	{
		OutError = TEXT("ProbeSamples must be at least 1");
		return false;
	}

	const std::string Url = aurix::Regions::discovery_url(
		ToUtf8(Request.ApiUrl.TrimStartAndEnd()), ToUtf8(Request.PreferredRegion.TrimStartAndEnd()),
		Request.bHasLocation ? &Request.Latitude : nullptr, Request.bHasLocation ? &Request.Longitude : nullptr);
	if (Url.empty())
	{
		OutError = FString::Printf(TEXT("invalid discovery request: %s"), *FromUtf8(aurix::last_error().c_str()));
		return false;
	}

	ListRequest = FHttpModule::Get().CreateRequest();
	ListRequest->SetURL(FromUtf8(Url.c_str()));
	ListRequest->SetVerb(TEXT("GET"));
	ListRequest->SetHeader(TEXT("Accept"), TEXT("application/json"));
	ListRequest->SetHeader(TEXT("Authorization"), FString::Printf(TEXT("Bearer %s"), *Request.Token));
	ListRequest->SetTimeout(FMath::Max(MinListTimeoutSeconds, Request.ProbeTimeoutSeconds));
	ListRequest->OnProcessRequestComplete().BindSP(this, &FAurixRegionDiscovery::OnListReceived);
	if (!ListRequest->ProcessRequest())
	{
		ListRequest->OnProcessRequestComplete().Unbind();
		ListRequest.Reset();
		OutError = TEXT("failed to start the regions request");
		return false;
	}
	return true;
}

void FAurixRegionDiscovery::Cancel()
{
	bDone = true;
	if (ListRequest.IsValid())
	{
		ListRequest->OnProcessRequestComplete().Unbind();
		ListRequest->CancelRequest();
		ListRequest.Reset();
	}
	for (FProbe& Probe : Probes)
	{
		if (Probe.Request.IsValid())
		{
			Probe.Request->OnProcessRequestComplete().Unbind();
			Probe.Request->CancelRequest();
			Probe.Request.Reset();
		}
	}
}

void FAurixRegionDiscovery::OnListReceived(FHttpRequestPtr HttpRequest, FHttpResponsePtr Response, bool bConnectedSuccessfully)
{
	ListRequest.Reset();
	if (bDone)
	{
		return;
	}
	if (!bConnectedSuccessfully || !Response.IsValid())
	{
		Finish(false, TEXT("regions request failed: no response"));
		return;
	}
	const int32 Code = Response->GetResponseCode();
	if (Code != 200)
	{
		Finish(false, FString::Printf(TEXT("regions request failed: HTTP %d"), Code));
		return;
	}

	Regions = aurix::Regions::parse(ToUtf8(Response->GetContentAsString()));
	if (!Regions.valid())
	{
		Finish(false, FString::Printf(TEXT("regions response is malformed: %s"), *FromUtf8(aurix::last_error().c_str())));
		return;
	}

	if (Request.bProbe)
	{
		const std::vector<AurixRegionEndpoint> All = Regions.all();
		for (std::size_t i = 0; i < All.size(); ++i)
		{
			if (All[i].probe_url[0] == '\0')
			{
				continue;
			}
			FProbe Probe;
			Probe.RegionIndex = static_cast<int32>(i);
			Probe.Url = FromUtf8(All[i].probe_url);
			Probe.SamplesLeft = Request.ProbeSamples;
			Probes.Add(MoveTemp(Probe));
		}
	}
	if (Probes.Num() == 0)
	{
		RankAndFinish();
		return;
	}

	PendingProbes = Probes.Num();
	for (int32 i = 0; i < Probes.Num(); ++i)
	{
		SendProbe(i);
	}
}

void FAurixRegionDiscovery::SendProbe(int32 ProbeIndex)
{
	FProbe& Probe = Probes[ProbeIndex];
	Probe.Request = FHttpModule::Get().CreateRequest();
	Probe.Request->SetURL(Probe.Url);
	Probe.Request->SetVerb(TEXT("GET"));
	Probe.Request->SetHeader(TEXT("Cache-Control"), TEXT("no-cache"));
	Probe.Request->SetTimeout(FMath::Max(0.1f, Request.ProbeTimeoutSeconds));
	Probe.Request->OnProcessRequestComplete().BindSP(this, &FAurixRegionDiscovery::OnProbeReceived, ProbeIndex);
	Probe.StartedAt = FPlatformTime::Seconds();
	if (!Probe.Request->ProcessRequest())
	{
		// Count it as a failed sample and move on so the discovery still completes.
		Probe.Request->OnProcessRequestComplete().Unbind();
		OnProbeReceived(Probe.Request, nullptr, false, ProbeIndex);
	}
}

void FAurixRegionDiscovery::OnProbeReceived(FHttpRequestPtr HttpRequest, FHttpResponsePtr Response, bool bConnectedSuccessfully, int32 ProbeIndex)
{
	if (bDone || !Probes.IsValidIndex(ProbeIndex))
	{
		return;
	}
	FProbe& Probe = Probes[ProbeIndex];
	const double ElapsedMs = (FPlatformTime::Seconds() - Probe.StartedAt) * 1000.0;
	Probe.Request.Reset();

	if (!Probe.bWarmupDone)
	{
		// The first request pays for DNS/TCP/TLS; measure only warm connections.
		Probe.bWarmupDone = true;
	}
	else
	{
		if (IsReachable(Response, bConnectedSuccessfully))
		{
			Probe.BestMs = Probe.bAnySuccess ? FMath::Min(Probe.BestMs, ElapsedMs) : ElapsedMs;
			Probe.bAnySuccess = true;
		}
		--Probe.SamplesLeft;
	}

	if (Probe.SamplesLeft > 0)
	{
		SendProbe(ProbeIndex);
		return;
	}
	if (--PendingProbes == 0)
	{
		RankAndFinish();
	}
}

void FAurixRegionDiscovery::RankAndFinish()
{
	for (const FProbe& Probe : Probes)
	{
		Regions.set_rtt(static_cast<std::size_t>(Probe.RegionIndex), Probe.bAnySuccess ? Probe.BestMs : -1.0);
	}
	const AurixResult Ranked = Regions.rank(ToUtf8(Request.PreferredRegion.TrimStartAndEnd()), Request.RttToleranceMs);
	if (Ranked != AURIX_OK)
	{
		Finish(false, FString::Printf(TEXT("ranking failed: %s"), *FromUtf8(aurix::last_error().c_str())));
		return;
	}
	Finish(true, FString());
}

void FAurixRegionDiscovery::Finish(bool bSuccess, const FString& Error)
{
	// The owner may drop its reference inside OnComplete; stay alive until we return.
	TSharedRef<FAurixRegionDiscovery> KeepAlive = AsShared();
	bDone = true;
	TArray<FAurixRegionEndpoint> Out;
	if (bSuccess)
	{
		for (const AurixRegionEndpoint& R : Regions.all())
		{
			Out.Add(ToEndpoint(R));
		}
	}
	else
	{
		UE_LOG(LogAurixVoice, Warning, TEXT("Region discovery failed: %s"), *Error);
	}
	OnComplete.ExecuteIfBound(bSuccess, Out, Error);
}
