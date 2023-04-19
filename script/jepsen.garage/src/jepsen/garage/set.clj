(ns jepsen.garage.set
  (:require [clojure.tools.logging :refer :all]
            [clojure.string :as str]
            [jepsen [checker :as checker]
             [cli :as cli]
             [client :as client]
             [control :as c]
             [db :as db]
             [generator :as gen]
             [independent :as independent]
             [nemesis :as nemesis]
             [tests :as tests]]
            [jepsen.checker.timeline :as timeline]
            [jepsen.control.util :as cu]
            [jepsen.os.debian :as debian]
            [jepsen.garage.grg :as grg]
            [knossos.model :as model]
            [slingshot.slingshot :refer [try+]]))

(defn op-add [_ _] {:type :invoke, :f :add, :value (rand-int 100000)})
(defn op-read [_ _] {:type :invoke, :f :read, :value nil})

(defrecord SetClient [creds]
  client/Client
  (open! [this test node]
    (let [creds (grg/s3-creds node)]
      (info node "s3 credentials:" creds)
      (assoc this :creds creds)))
  (setup! [this test])
  (invoke! [this test op]
    (case (:f op)
      :add
        (do
          (grg/s3-put (:creds this) (str (:value op)) "present")
          (assoc op :type :ok))
      :read
        (let [items (grg/s3-list (:creds this))]
          (assoc op :type :ok, :value (set (map read-string items))))))
  (teardown! [this test])
  (close! [this test]))

(defn workload
  "Tests insertions and deletions"
  [opts]
  {:client            (SetClient. nil)
   :checker           (checker/compose
                        {:set (checker/set)
                         :timeline (timeline/html)})
   ; :generator         (gen/mix [op-add op-read])
   ; :generator         (->> (range)
   ;                         (map (fn [x] {:type :invoke, :f :add, :value x})))
   :generator         (gen/mix [op-read
                        (->> (range) (map (fn [x] {:type :invoke, :f :add, :value x})))])
   :final-generator   (gen/once op-read)})

