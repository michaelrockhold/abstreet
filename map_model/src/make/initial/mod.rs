mod fix_ramps;
mod geometry;
pub mod lane_specs;
mod merge;

use crate::raw_data::{StableIntersectionID, StableRoadID};
use crate::{raw_data, IntersectionType, LANE_THICKNESS};
use abstutil::Timer;
use geom::{Bounds, Distance, GPSBounds, PolyLine, Pt2D};
use serde_derive::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub struct InitialMap {
    pub roads: BTreeMap<StableRoadID, Road>,
    pub intersections: BTreeMap<StableIntersectionID, Intersection>,

    pub name: String,
    pub bounds: Bounds,
}

pub struct Road {
    pub id: StableRoadID,
    pub src_i: StableIntersectionID,
    pub dst_i: StableIntersectionID,
    pub original_center_pts: PolyLine,
    pub trimmed_center_pts: PolyLine,
    pub fwd_width: Distance,
    pub back_width: Distance,
    pub lane_specs: Vec<lane_specs::LaneSpec>,
}

impl Road {
    pub fn original_endpoint(&self, i: StableIntersectionID) -> Pt2D {
        if self.src_i == i {
            self.original_center_pts.first_pt()
        } else if self.dst_i == i {
            self.original_center_pts.last_pt()
        } else {
            panic!("{} doesn't end at {}", self.id, i);
        }
    }
}

pub struct Intersection {
    pub id: StableIntersectionID,
    pub polygon: Vec<Pt2D>,
    pub roads: BTreeSet<StableRoadID>,
    pub intersection_type: IntersectionType,
}

impl InitialMap {
    pub fn new(
        name: String,
        data: &raw_data::Map,
        gps_bounds: &GPSBounds,
        bounds: &Bounds,
        timer: &mut Timer,
    ) -> InitialMap {
        let mut m = InitialMap {
            roads: BTreeMap::new(),
            intersections: BTreeMap::new(),
            name,
            bounds: bounds.clone(),
        };

        for (stable_id, i) in &data.intersections {
            m.intersections.insert(
                *stable_id,
                Intersection {
                    id: *stable_id,
                    polygon: Vec::new(),
                    roads: BTreeSet::new(),
                    intersection_type: i.intersection_type,
                },
            );
        }

        for (stable_id, r) in &data.roads {
            if r.i1 == r.i2 {
                timer.warn(format!(
                    "OSM way {} is a loop on {}, skipping what would've been {}",
                    r.osm_way_id, r.i1, stable_id
                ));
                continue;
            }
            m.intersections
                .get_mut(&r.i1)
                .unwrap()
                .roads
                .insert(*stable_id);
            m.intersections
                .get_mut(&r.i2)
                .unwrap()
                .roads
                .insert(*stable_id);

            let original_center_pts = PolyLine::new(gps_bounds.must_convert(&r.points));

            let lane_specs = lane_specs::get_lane_specs(r, *stable_id);
            let mut fwd_width = Distance::ZERO;
            let mut back_width = Distance::ZERO;
            for l in &lane_specs {
                if l.reverse_pts {
                    back_width += LANE_THICKNESS;
                } else {
                    fwd_width += LANE_THICKNESS;
                }
            }

            // TODO I can't find anything online that describes how to interpret the given OSM
            // geometry of one-ways. I'm interpreting the way as the edge of the road (and only
            // shift_right()ing from there). But could also uncomment this and interpret the way as
            // the actual center of the one-way road. It looks quite bad -- dual carriageways get
            // smooshed together.
            /*assert_ne!(fwd_width, Distance::ZERO);
            if back_width == Distance::ZERO {
                // Interpret the original OSM geometry of one-ways as the actual center of the
                // road.
                original_center_pts = original_center_pts.shift_left(fwd_width / 2.0);
            }*/

            m.roads.insert(
                *stable_id,
                Road {
                    id: *stable_id,
                    src_i: r.i1,
                    dst_i: r.i2,
                    original_center_pts: original_center_pts.clone(),
                    trimmed_center_pts: original_center_pts,
                    fwd_width,
                    back_width,
                    lane_specs,
                },
            );
        }

        timer.start_iter("find each intersection polygon", m.intersections.len());
        for i in m.intersections.values_mut() {
            timer.next();

            i.polygon = geometry::intersection_polygon(i, &mut m.roads, timer);
        }

        fix_ramps::fix_ramps(&mut m, timer);

        merge::short_roads(&mut m, timer);

        m
    }

    pub fn merge_road(&mut self, r: StableRoadID) {
        merge::merge(self, r, &mut Timer::throwaway());
    }

    pub fn delete_road(&mut self, r: StableRoadID) {
        let mut timer = Timer::throwaway();
        let road = self.roads.remove(&r).unwrap();
        {
            let mut i = self.intersections.get_mut(&road.src_i).unwrap();
            i.roads.remove(&r);
            i.polygon = geometry::intersection_polygon(i, &mut self.roads, &mut timer);
        }
        {
            let mut i = self.intersections.get_mut(&road.dst_i).unwrap();
            i.roads.remove(&r);
            i.polygon = geometry::intersection_polygon(i, &mut self.roads, &mut timer);
        }
    }

    pub fn merge_degenerate_intersection(&mut self, delete_i: StableIntersectionID) {
        let mut timer = Timer::throwaway();
        let (r1, r2) = {
            let i = self.intersections.remove(&delete_i).unwrap();
            let roads: Vec<StableRoadID> = i.roads.into_iter().collect();
            assert_eq!(roads.len(), 2);
            (roads[0], roads[1])
        };
        // new_i1 is the other end of r1, new_i2 is the other end of r2
        let new_i1 = {
            let r = &self.roads[&r1];
            if r.src_i == delete_i {
                r.dst_i
            } else {
                r.src_i
            }
        };
        let new_i2 = {
            let r = &self.roads[&r2];
            if r.src_i == delete_i {
                r.dst_i
            } else {
                r.src_i
            }
        };

        // Arbitrarily delete r1. Fix up intersections
        let deleted_road = self.roads.remove(&r1).unwrap();
        {
            let i = self.intersections.get_mut(&new_i1).unwrap();
            i.roads.remove(&r1);
            i.roads.insert(r2);
        }
        // Start at delete_i and go to new_i1.
        let pts_towards_new_i1 = if deleted_road.src_i == delete_i {
            deleted_road.original_center_pts
        } else {
            deleted_road.original_center_pts.reversed()
        };

        // Fix up r2.
        {
            let r = self.roads.get_mut(&r2).unwrap();
            if r.src_i == delete_i {
                r.src_i = new_i1;
                r.original_center_pts = pts_towards_new_i1
                    .reversed()
                    .extend(r.original_center_pts.clone());
            } else {
                r.dst_i = new_i1;
                r.original_center_pts = r.original_center_pts.clone().extend(pts_towards_new_i1);
            }
            r.trimmed_center_pts = r.original_center_pts.clone();
        }

        // And finally the intersection geometry
        {
            let i = self.intersections.get_mut(&new_i1).unwrap();
            i.polygon = geometry::intersection_polygon(i, &mut self.roads, &mut timer);
        }
        {
            let i = self.intersections.get_mut(&new_i2).unwrap();
            i.polygon = geometry::intersection_polygon(i, &mut self.roads, &mut timer);
        }
    }

    pub fn apply_hints(&mut self, hints: &Hints) {
        for h in &hints.hints {
            match h {
                Hint::MergeRoad(r) => {
                    self.merge_road(*r);
                }
                Hint::DeleteRoad(r) => {
                    self.delete_road(*r);
                }
                Hint::MergeDegenerateIntersection(i) => {
                    self.merge_degenerate_intersection(*i);
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Hints {
    pub hints: Vec<Hint>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Hint {
    MergeRoad(StableRoadID),
    DeleteRoad(StableRoadID),
    MergeDegenerateIntersection(StableIntersectionID),
}
