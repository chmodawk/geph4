use crate::mux::structs::*;
use std::{
    cmp::Reverse,
    collections::BTreeSet,
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct InflightEntry {
    seqno: Seqno,
    acked: bool,
    send_time: Instant,
    pub retrans: u64,
    pub payload: Message,

    delivered: u64,
    delivered_time: Instant,
}

pub struct Inflight {
    segments: VecDeque<InflightEntry>,
    inflight_count: usize,
    times: priority_queue::PriorityQueue<Seqno, Reverse<Instant>>,
    fast_retrans: BTreeSet<Seqno>,
    rtt: RttCalculator,
    rate: RateCalculator,

    delivered: u64,
    delivered_time: Instant,
}

impl Inflight {
    pub fn new() -> Self {
        Inflight {
            segments: VecDeque::new(),
            inflight_count: 0,
            times: priority_queue::PriorityQueue::new(),
            fast_retrans: BTreeSet::new(),
            rtt: RttCalculator::default(),
            rate: RateCalculator::default(),

            delivered: 0,
            delivered_time: Instant::now(),
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate.rate
    }

    pub fn bdp(&self) -> f64 {
        self.rate() * self.min_rtt().as_secs_f64()
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn inflight(&self) -> usize {
        if self.inflight_count > self.segments.len() {
            panic!(
                "inflight_count = {}, segment len = {}",
                self.inflight_count,
                self.segments.len()
            );
        }
        self.inflight_count
    }

    pub fn srtt(&self) -> Duration {
        Duration::from_millis(self.rtt.srtt)
    }

    pub fn min_rtt(&self) -> Duration {
        Duration::from_millis(self.rtt.min_rtt)
    }

    pub fn mark_acked_lt(&mut self, seqno: Seqno) {
        for segseq in self.segments.iter().map(|v| v.seqno).collect::<Vec<_>>() {
            if segseq < seqno {
                self.mark_acked(segseq);
            } else {
                break;
            }
        }
    }

    pub fn mark_acked(&mut self, seqno: Seqno) -> bool {
        let mut toret = false;
        let now = Instant::now();
        // mark the right one
        if let Some(entry) = self.segments.front() {
            let first_seqno = entry.seqno;
            if seqno >= first_seqno {
                let offset = (seqno - first_seqno) as usize;
                if let Some(seg) = self.segments.get_mut(offset) {
                    if !seg.acked {
                        self.delivered += 1;
                        self.delivered_time = now;
                        toret = true;
                        seg.acked = true;
                        self.inflight_count -= 1;
                        if seg.retrans == 0 {
                            if let Message::Rel { .. } = &seg.payload {
                                // calculate rate
                                let data_acked = self.delivered - seg.delivered;
                                let ack_elapsed = self
                                    .delivered_time
                                    .saturating_duration_since(seg.delivered_time);
                                let rate_sample = data_acked as f64 / ack_elapsed.as_secs_f64();
                                self.rate.record_sample(rate_sample)
                            }
                        }

                        self.rtt.record_sample(if seg.retrans == 0 {
                            Some(now.saturating_duration_since(seg.send_time))
                        } else {
                            None
                        });
                    }
                }
                // shrink if possible
                while self.len() > 0 && self.segments.front().unwrap().acked {
                    self.segments.pop_front();
                }
            }
        }
        toret
    }

    pub fn insert(&mut self, seqno: Seqno, msg: Message) {
        let rto = self.rtt.rto();
        if self.get_seqno(seqno).is_none() {
            self.segments.push_back(InflightEntry {
                seqno,
                acked: false,
                send_time: Instant::now(),
                payload: msg,
                retrans: 0,
                delivered: self.delivered,
                delivered_time: self.delivered_time,
            });
            self.inflight_count += 1;
        }
        self.times.push(seqno, Reverse(Instant::now() + rto));
    }

    pub fn get_seqno(&mut self, seqno: Seqno) -> Option<&mut InflightEntry> {
        if let Some(first_entry) = self.segments.front() {
            let first_seqno = first_entry.seqno;
            if seqno >= first_seqno {
                let offset = (seqno - first_seqno) as usize;
                return self.segments.get_mut(offset);
            }
        }
        None
    }

    pub async fn wait_first(&mut self) -> Option<(Seqno, bool)> {
        if let Some(seq) = self.fast_retrans.iter().next() {
            let seq = *seq;
            self.fast_retrans.remove(&seq);
            return Some((seq, false));
        }
        while !self.times.is_empty() {
            let (_, time) = self.times.peek().unwrap();
            let durat = time.0.saturating_duration_since(Instant::now());
            if durat.as_secs() > 30 {
                return None;
            }
            smol::Timer::at(time.0).await;
            let (seqno, _) = self.times.pop().unwrap();
            let mut rto = self.rtt.rto();
            if let Some(seg) = self.get_seqno(seqno) {
                if !seg.acked {
                    seg.retrans += 1;
                    let rtx = seg.retrans;
                    for _ in 0..rtx {
                        rto *= 3;
                        rto /= 2
                    }

                    self.times.push(seqno, Reverse(Instant::now() + rto));
                    return Some((seqno, true));
                }
            }
        }
        smol::future::pending().await
    }
}

struct RateCalculator {
    rate: f64,
    rate_update_time: Instant,
}

impl Default for RateCalculator {
    fn default() -> Self {
        RateCalculator {
            rate: 100.0,
            rate_update_time: Instant::now(),
        }
    }
}

impl RateCalculator {
    fn record_sample(&mut self, sample: f64) {
        let now = Instant::now();
        if now
            .saturating_duration_since(self.rate_update_time)
            .as_secs()
            > 3
            || sample > self.rate
        {
            self.rate = sample;
            self.rate_update_time = now;
        }
    }
}

struct RttCalculator {
    // standard TCP stuff
    srtt: u64,
    rttvar: u64,
    rto: u64,

    // rate estimation
    min_rtt: u64,
    rtt_update_time: Instant,

    existing: bool,
}

impl Default for RttCalculator {
    fn default() -> Self {
        RttCalculator {
            srtt: 300,
            rttvar: 0,
            rto: 300,
            min_rtt: 300,
            rtt_update_time: Instant::now(),
            existing: false,
        }
    }
}

impl RttCalculator {
    fn record_sample(&mut self, sample: Option<Duration>) {
        if let Some(sample) = sample {
            let sample = sample.as_millis() as u64;
            if !self.existing {
                self.srtt = sample;
                self.rttvar = sample / 2;
            } else {
                self.rttvar = self.rttvar * 3 / 4 + diff(self.srtt, sample) / 4;
                self.srtt = self.srtt * 7 / 8 + sample / 8;
            }
            self.rto = sample.max(self.srtt + (4 * self.rttvar).max(10)) + 50;
        }
        // delivery rate
        let now = Instant::now();
        if self.srtt < self.min_rtt
            || now
                .saturating_duration_since(self.rtt_update_time)
                .as_millis()
                > 10000
        {
            self.min_rtt = self.srtt;
            self.rtt_update_time = now;
        }
    }

    fn rto(&self) -> Duration {
        Duration::from_millis(self.rto)
    }
}

fn diff(a: u64, b: u64) -> u64 {
    if b > a {
        b - a
    } else {
        a - b
    }
}
