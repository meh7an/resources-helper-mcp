using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;

using ResourcesHelper.Tray.Core.Contract;

namespace ResourcesHelper.Tray.Core.Tests;

[TestClass]
public sealed class ContractFixtureTests
{
    private static readonly string FixtureDirectory =
        Path.Combine(AppContext.BaseDirectory, "Fixtures");

    // Production ignores unknown members, as serde does; in a fixture one would mean the
    // generated types miss a field.
    private static readonly JsonTypeInfo<UiState> NoUnknownMembers =
        (JsonTypeInfo<UiState>)new JsonSerializerOptions(ContractJsonContext.Default.Options)
        {
            UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
        }.GetTypeInfo(typeof(UiState));

    public static IEnumerable<object[]> FixtureFiles =>
        Directory.GetFiles(FixtureDirectory, "*.json")
            .Order(StringComparer.Ordinal)
            .Select(path => new object[] { Path.GetFileName(path) });

    [TestMethod]
    [DynamicData(nameof(FixtureFiles))]
    public void EveryFixtureDecodesWithoutUnknownMembers(string fileName)
    {
        var json = File.ReadAllText(Path.Combine(FixtureDirectory, fileName));

        Decode(json, NoUnknownMembers);
    }

    [TestMethod]
    public void BusyFixtureKeepsJobsQueueAndNotice()
    {
        var state = DecodeFixture("busy");

        Assert.HasCount(2, state.Active);
        Assert.AreEqual(JobClass.GpuHeavy, state.Active[0].Lease.Class);
        Assert.IsFalse(state.Active[0].Process?.Bound);
        Assert.IsNull(state.Active[0].Process?.Exe);
        Assert.IsTrue(state.Active[1].Process?.Bound);
        Assert.AreEqual(LeaseState.Active, state.Active[1].Lease.State);
        Assert.IsNull(state.Active[1].Lease.EndCause);
        Assert.IsNull(state.Active[1].Lease.Binding);
        Assert.AreEqual(120UL, state.Queue[0].Verdict.WaitEstimateSecs);
        Assert.IsNull(state.Queue[1].Verdict.WaitEstimateSecs);
        Assert.IsNull(state.Queue[1].Lease.Class);
        Assert.IsNull(state.Sessions[2].Cwd);
        Assert.AreEqual(EndCause.Killed, state.Notices[0].Cause);
        Assert.IsNull(state.Notices[0].DeliveredAtMs);
    }

    [TestMethod]
    public void BenchmarkFixtureDecodesItsPhase()
    {
        var state = DecodeFixture("benchmark");

        Assert.AreEqual(LeaseKind.BenchmarkExclusive, state.Queue[0].Lease.Kind);
        var measuring = state.Queue[0].Verdict.Benchmark as BenchmarkProgress.Measuring;
        Assert.IsNotNull(measuring);
        Assert.AreEqual(6UL, measuring.MeasuredSecs);
        Assert.AreEqual(10UL, measuring.NeededSecs);
        Assert.IsNull(state.Queue[1].Verdict.Benchmark);
        Assert.HasCount(2, state.Meters.Gpus);
        Assert.IsNull(state.Meters.Gpus[1].VramTotalMb);
    }

    [TestMethod]
    public void IdleFixtureKeepsRunsFromBeforeSessionsAndCauses()
    {
        var state = DecodeFixture("idle");

        Assert.IsEmpty(state.Active);
        Assert.AreEqual(EndCause.Killed, state.Recent[1].EndCause);
        Assert.IsFalse(state.Recent[2].Noise?.QuietAtStart);
        Assert.IsNull(state.Recent[3].LeaseId);
        Assert.IsNull(state.Recent[3].Session);
        Assert.IsNull(state.Recent[3].EndCause);
        Assert.IsNotNull(state.Notices[0].DeliveredAtMs);
    }

    [TestMethod]
    public void FirstRunFixtureHasNoFirstSession()
    {
        var state = DecodeFixture("first-run");

        Assert.IsNull(state.FirstSessionAtMs);
        Assert.IsEmpty(state.Sessions);
        Assert.IsEmpty(state.Meters.Gpus);
        Assert.IsEmpty(state.Meters.Drives);
    }

    [TestMethod]
    public void MissingLeaseCwdDecodesAsEmpty()
    {
        var root = FixtureNode("busy");
        root["active"]![0]!["lease"]!.AsObject().Remove("cwd");

        var state = Decode(root.ToJsonString(), ContractJsonContext.Default.UiState);

        Assert.AreEqual("", state.Active[0].Lease.Cwd);
    }

    [TestMethod]
    public void BenchmarkPhaseMayFollowItsFields()
    {
        var root = FixtureNode("benchmark");
        var benchmark = root["queue"]![0]!["verdict"]!["benchmark"]!.AsObject();
        var phase = benchmark["phase"]!.GetValue<string>();
        benchmark.Remove("phase");
        benchmark["phase"] = phase;

        var state = Decode(root.ToJsonString(), ContractJsonContext.Default.UiState);

        Assert.IsInstanceOfType<BenchmarkProgress.Measuring>(state.Queue[0].Verdict.Benchmark);
    }

    [TestMethod]
    public void MissingRequiredMemberIsRejected()
    {
        var root = FixtureNode("busy");
        root.Remove("generated_at_ms");

        AssertRejected(root);
    }

    [TestMethod]
    public void NullRequiredStringIsRejected()
    {
        var root = FixtureNode("busy");
        root["service"]!["version"] = null;

        AssertRejected(root);
    }

    [TestMethod]
    public void UnknownEnumNameIsRejected()
    {
        var root = FixtureNode("busy");
        root["active"]![0]!["lease"]!["state"] = "paused";

        AssertRejected(root);
    }

    [TestMethod]
    public void NumericEnumValueIsRejected()
    {
        var root = FixtureNode("busy");
        root["active"]![0]!["lease"]!["state"] = 1;

        AssertRejected(root);
    }

    [TestMethod]
    public void UnknownBenchmarkPhaseIsRejected()
    {
        var root = FixtureNode("benchmark");
        root["queue"]![0]!["verdict"]!["benchmark"]!["phase"] = "warming";

        AssertRejected(root);
    }

    private static string ReadFixture(string scenario) =>
        File.ReadAllText(Path.Combine(FixtureDirectory, $"ui-state-{scenario}.json"));

    private static JsonObject FixtureNode(string scenario) =>
        JsonNode.Parse(ReadFixture(scenario))!.AsObject();

    private static UiState DecodeFixture(string scenario) =>
        Decode(ReadFixture(scenario), ContractJsonContext.Default.UiState);

    private static UiState Decode(string json, JsonTypeInfo<UiState> typeInfo)
    {
        var state = JsonSerializer.Deserialize(json, typeInfo);
        Assert.IsNotNull(state);
        return state;
    }

    private static void AssertRejected(JsonObject root) =>
        Assert.ThrowsExactly<JsonException>(
            () => JsonSerializer.Deserialize(
                root.ToJsonString(), ContractJsonContext.Default.UiState));
}
