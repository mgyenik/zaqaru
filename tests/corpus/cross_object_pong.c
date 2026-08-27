/* Milestone 4, the payoff demo (second half). See cross_object_ping.c. */

int ping(int steps);

extern int shared_total;

int pong(int steps) {
	shared_total += 100;
	if (steps <= 0) {
		return 0;
	}
	return 10 + ping(steps - 1);
}
