namespace x264video4osu.Services;

/// <summary>
/// 编码模式枚举
/// </summary>
public enum EncodeMode
{
    /// <summary>
    /// 两遍编码（2pass平均码率）
    /// </summary>
    TwoPass,

    /// <summary>
    /// CRF恒定质量模式（单次编码）
    /// </summary>
    CRF
}

public class EncodeOptions
{
    public string InputPath { get; set; } = "";
    public string OutputPath { get; set; } = "";
    public int BitrateK { get; set; }
    public int Width { get; set; }
    public int Height { get; set; }
    public bool AllowScaleUp { get; set; }
    public bool ExtractAudio { get; set; }
    
    /// <summary>
    /// 编码模式：2pass 或 CRF
    /// </summary>
    public EncodeMode Mode { get; set; } = EncodeMode.TwoPass;
    
    /// <summary>
    /// CRF值（仅CRF模式使用）
    /// </summary>
    public int CrfValue { get; set; } = 23;
}
