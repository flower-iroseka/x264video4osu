namespace x264video4osu.Services;

public class ProgressInfo
{
    public int PassIndex { get; set; }
    public int PassCount { get; set; }
    public double Percent { get; set; }
    public TimeSpan Elapsed { get; set; }
    public TimeSpan EstimatedRemaining { get; set; }
}