// The official Java Bolt driver against `mushroomdb serve --bolt-port`: connect
// with the token, run `MATCH (n) RETURN count(n)`, run one explicit
// transaction (begin, create, commit, read back), disconnect. Exits 1 on any
// error. Run through tests/bolt/extra_driver.sh, which fills @VENDOR@ in.
import org.@VENDOR@.driver.AuthTokens;
import org.@VENDOR@.driver.Driver;
import org.@VENDOR@.driver.GraphDatabase;
import org.@VENDOR@.driver.Record;
import org.@VENDOR@.driver.Session;
import org.@VENDOR@.driver.Transaction;

public class CompatCheck {
    static void check(boolean condition, String message) {
        if (!condition) {
            throw new IllegalStateException(message);
        }
    }

    public static void main(String[] args) {
        String uri = args[0];
        String token = System.getenv("BOLT_COMPAT_TOKEN");
        try (Driver driver = GraphDatabase.driver(uri, AuthTokens.basic("compat", token))) {
            driver.verifyConnectivity();
            try (Session session = driver.session()) {
                Record record = session.run("MATCH (n) RETURN count(n) AS c").single();
                check(record.get("c").asObject() instanceof Long, "count(n) is not an integer");
            }
            try (Session session = driver.session()) {
                try (Transaction tx = session.beginTransaction()) {
                    tx.run("CREATE (:Person {id: 'java-tx', name: 'Grace'})").consume();
                    tx.commit();
                }
                Record seen = session.run(
                        "MATCH (p:Person {id: 'java-tx'}) RETURN count(p) AS c").single();
                check(seen.get("c").asLong() == 1L, "committed node not read back");
            }
            System.out.println("java driver: connect, read and transaction passed");
        } catch (Exception err) {
            err.printStackTrace();
            System.exit(1);
        }
    }
}
