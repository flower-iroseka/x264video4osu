namespace x264video4osu.Services;

public static class ScaleHelper
{
    public static string Build(int w, int h, bool upscale)
    {
        if (w == 0 && h == 0)
            return "scale=iw:ih";

        if (w == 0)
            return upscale
                ? $"scale=-1:{h}"
                : $"scale=-1:'if(gt(ih,{h}),{h},ih)'";

        if (h == 0)
            return upscale
                ? $"scale={w}:-1"
                : $"scale='if(gt(iw,{w}),{w},iw)':-1";

        return upscale
            ? $"scale={w}:{h}"
            : $"scale='if(gt(iw,{w}),{w},iw)':'if(gt(ih,{h}),{h},ih)'";
    }
}