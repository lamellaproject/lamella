// Lamella managed corlib (from scratch). -- System.Func<TResult> .. System.Func<T1..T16,TResult>
#if LAMELLA_SURFACE_NETFX_4_0
namespace System
{
    /// <summary>A method that takes no arguments and returns a value.</summary>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<TResult>();

    /// <summary>A method that takes one argument and returns a value.</summary>
    /// <typeparam name="T">The type of the argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T, TResult>(T arg);

    /// <summary>A method that takes two arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, TResult>(T1 arg1, T2 arg2);

    /// <summary>A method that takes three arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, TResult>(T1 arg1, T2 arg2, T3 arg3);

    /// <summary>A method that takes four arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4);

    /// <summary>A method that takes five arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5);

    /// <summary>A method that takes six arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6);

    /// <summary>A method that takes seven arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7);

    /// <summary>A method that takes eight arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8);

    /// <summary>A method that takes nine arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9);

    /// <summary>A method that takes ten arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10);

    /// <summary>A method that takes eleven arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="T11">The type of the eleventh argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10, T11 arg11);

    /// <summary>A method that takes twelve arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="T11">The type of the eleventh argument.</typeparam>
    /// <typeparam name="T12">The type of the twelfth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10, T11 arg11, T12 arg12);

    /// <summary>A method that takes thirteen arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="T11">The type of the eleventh argument.</typeparam>
    /// <typeparam name="T12">The type of the twelfth argument.</typeparam>
    /// <typeparam name="T13">The type of the thirteenth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10, T11 arg11, T12 arg12, T13 arg13);

    /// <summary>A method that takes fourteen arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="T11">The type of the eleventh argument.</typeparam>
    /// <typeparam name="T12">The type of the twelfth argument.</typeparam>
    /// <typeparam name="T13">The type of the thirteenth argument.</typeparam>
    /// <typeparam name="T14">The type of the fourteenth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10, T11 arg11, T12 arg12, T13 arg13, T14 arg14);

    /// <summary>A method that takes fifteen arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="T11">The type of the eleventh argument.</typeparam>
    /// <typeparam name="T12">The type of the twelfth argument.</typeparam>
    /// <typeparam name="T13">The type of the thirteenth argument.</typeparam>
    /// <typeparam name="T14">The type of the fourteenth argument.</typeparam>
    /// <typeparam name="T15">The type of the fifteenth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10, T11 arg11, T12 arg12, T13 arg13, T14 arg14, T15 arg15);

    /// <summary>A method that takes sixteen arguments and returns a value.</summary>
    /// <typeparam name="T1">The type of the first argument.</typeparam>
    /// <typeparam name="T2">The type of the second argument.</typeparam>
    /// <typeparam name="T3">The type of the third argument.</typeparam>
    /// <typeparam name="T4">The type of the fourth argument.</typeparam>
    /// <typeparam name="T5">The type of the fifth argument.</typeparam>
    /// <typeparam name="T6">The type of the sixth argument.</typeparam>
    /// <typeparam name="T7">The type of the seventh argument.</typeparam>
    /// <typeparam name="T8">The type of the eighth argument.</typeparam>
    /// <typeparam name="T9">The type of the ninth argument.</typeparam>
    /// <typeparam name="T10">The type of the tenth argument.</typeparam>
    /// <typeparam name="T11">The type of the eleventh argument.</typeparam>
    /// <typeparam name="T12">The type of the twelfth argument.</typeparam>
    /// <typeparam name="T13">The type of the thirteenth argument.</typeparam>
    /// <typeparam name="T14">The type of the fourteenth argument.</typeparam>
    /// <typeparam name="T15">The type of the fifteenth argument.</typeparam>
    /// <typeparam name="T16">The type of the sixteenth argument.</typeparam>
    /// <typeparam name="TResult">The type of the value it returns.</typeparam>
    public delegate TResult Func<T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16, TResult>(T1 arg1, T2 arg2, T3 arg3, T4 arg4, T5 arg5, T6 arg6, T7 arg7, T8 arg8, T9 arg9, T10 arg10, T11 arg11, T12 arg12, T13 arg13, T14 arg14, T15 arg15, T16 arg16);
}
#endif
