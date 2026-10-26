// The official .NET Bolt driver against `mushroomdb serve --bolt-port`: connect
// with the token, run `MATCH (n) RETURN count(n)`, run one explicit
// transaction (begin, create, commit, read back), disconnect. Exits 1 on any
// error. Run through tests/bolt/extra_driver.sh, which fills @VENDOR_CAP@ in.
using @VENDOR_CAP@.Driver;

static async Task<long> CountAsync(Func<string, Task<IResultCursor>> run, string query)
{
    var cursor = await run(query);
    if (!await cursor.FetchAsync())
    {
        throw new InvalidOperationException($"no row for {query}");
    }
    var value = cursor.Current.Values.First().Value;
    if (value is not long count)
    {
        throw new InvalidOperationException($"{query} did not return an integer: {value?.GetType()}");
    }
    await cursor.ConsumeAsync();
    return count;
}

try
{
    var token = Environment.GetEnvironmentVariable("BOLT_COMPAT_TOKEN") ?? "";
    await using var driver = GraphDatabase.Driver(args[0], AuthTokens.Basic("compat", token));
    await driver.VerifyConnectivityAsync();

    var session = driver.AsyncSession();
    try
    {
        await CountAsync(q => session.RunAsync(q), "MATCH (n) RETURN count(n)");

        var tx = await session.BeginTransactionAsync();
        var created = await tx.RunAsync("CREATE (:Person {id: 'dotnet-tx', name: 'Grace'})");
        await created.ConsumeAsync();
        await tx.CommitAsync();

        var seen = await CountAsync(
            q => session.RunAsync(q), "MATCH (p:Person {id: 'dotnet-tx'}) RETURN count(p)");
        if (seen != 1)
        {
            throw new InvalidOperationException($"committed node not read back: {seen}");
        }
    }
    finally
    {
        await session.CloseAsync();
    }
    Console.WriteLine(".NET driver: connect, read and transaction passed");
}
catch (Exception err)
{
    Console.Error.WriteLine(err);
    Environment.Exit(1);
}
