#include <boost/program_options.hpp>
#include <boost/regex.hpp>
#include <chrono>
#include <ctime>
#include <exception>
#include <filesystem>
#include <fstream>
#include <limits>
#include <signal.h>
#include <stdio.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef RELATION
#include "../include/bound_sketch.h"
#include "../include/correlated_sampling.h"
#else
#include "../include/cset.h"
#include "../include/impr.h"
#include "../include/jsub.h"
#include "../include/sumrdf.h"
#include "../include/wander_join.h"
#endif
#include "../include/memory.h"

namespace po = boost::program_options;
typedef std::numeric_limits<double> dbl;
typedef std::chrono::high_resolution_clock Clock;

struct QueryResult {
  double est;
  double time;
  int m_est;
};

struct QueryParams {
  int num_iter;
  int seed;
  double ratio;

  QueryParams(int num_iter, int seed, double ratio)
      : num_iter(num_iter), seed(seed), ratio(ratio) {}
};

void query(Estimator *estimator, DataGraph &g, const QueryParams &query_params,
           QueryResult *query_result, const char *path) {
  int num_iter = query_params.num_iter;
  int seed = query_params.seed;
  double p = query_params.ratio;
  try {
    QueryGraph q;
    q.ReadText(path);
    vector<double> est_vec;
    double avg_est = 0.0, avg_time = 0.0;
    int num_est = 0;
    for (int i = 0; i < num_iter; i++) {
      // do fork
      int child_pid = fork();
      if (child_pid == 0) {
        srand(seed + i);
        auto chkpt = Clock::now();
        query_result->est = estimator->Run(g, q, p);
        auto elapsed = chrono::duration<double>(Clock::now() - chkpt);
        query_result->time =
            chrono::duration_cast<chrono::microseconds>(elapsed).count() / 1e6;
        query_result->m_est =
            std::max(query_result->m_est, getValueOfPhysicalMemoryUsage());
        shmdt(query_result);
        exit(EXIT_SUCCESS);
      } else if (child_pid > 0) {
        auto chkpt = Clock::now();
        while (true) {
          int child_status = 0;
          usleep(100000); // sleep 0.1 second
          int wait_result = waitpid(child_pid, &child_status, WNOHANG);
          if (wait_result != 0 && WIFEXITED(child_status)) {
            if (query_result->est > -1e9) {
              est_vec.push_back(query_result->est);
              avg_time += query_result->time;
            }
            break;
          } else if (wait_result != 0 && WIFSIGNALED(child_status)) {
            int signal = WTERMSIG(child_status);
            std::cerr << "child signaled exit " << WTERMSIG(child_status)
                      << "\n";
            throw signal;
            // throw Estimator::ErrCode::UNKNOWN;
          }
          auto elapsed = chrono::duration<double>(Clock::now() - chkpt);
          double elapsed_milliseconds =
              chrono::duration_cast<chrono::milliseconds>(elapsed).count();

          if (elapsed_milliseconds > 5 * 60 * 1000.0) {
            kill(child_pid, SIGKILL);
            std::cerr << "timeout\n";
            do {
              usleep(1000000); // sleep 1 second
              waitpid(child_pid, &child_status, WNOHANG);
            } while (!WIFEXITED(child_status) && !WIFSIGNALED(child_status));
            throw Estimator::ErrCode::TIMEOUT;
          }
        }
      } else {
        assert(false);
      }
    }
    for (double est : est_vec)
      avg_est += est;
    avg_est /= est_vec.size();
    avg_time /= est_vec.size();
    // double var = 0.0;
    // for (double est : est_vec)
    //   var += (est - avg_est) * (est - avg_est);
    // var /= est_vec.size();
    // int precision = std::numeric_limits<double>::max_digits10;
    // cout.precision(dbl::max_digits10);
    // fout.precision(dbl::max_digits10);
    // fout << dir_entry.path().string() << " " << avg_est << " " << avg_time
    //      << " " << var << endl;
    // cout << dir_entry.path().string() << " " << avg_est << " " << avg_time
    //      << " " << var << endl;
    // fout << est_vec.size();
    // cout << est_vec.size();
    // for (double est : est_vec)
    //   fout << " " << est;
    // for (double est : est_vec)
    //   cout << " " << est;
    // fout << endl;
    // cout << endl;
    // int precision = std::numeric_limits<double>::max_digits10;
    // cout.precision(dbl::max_digits10);
    cout << avg_est << "," << avg_time << "\n";
  } catch (Estimator::ErrCode e) {
    // err_fout << dir_entry.path().string() << " error with code " << e <<
    // "\n";
    cerr << path << " error with code " << e << "\n";
  } catch (int e) {
    // err_fout << dir_entry.path().string() << " error with the signal " << e
    // << "\n";
    cerr << path << " error with signal " << e << "\n";
  }
}

int main(int argc, char **argv) {

  po::options_description desc("gCare Framework");
  desc.add_options()("help,h", "Display help message")("query,q", "query mode")(
      "build,b", "build mode")("method,m", po::value<std::string>(),
                               "estimator method")(
      "input,i", po::value<std::string>(),
      "input file (in build mode: text data graph, in query mode: text query "
      "graph)")("output,o", po::value<std::string>(),
                "output directory in query mode")(
      "data,d", po::value<std::string>(), "binary datafile")(
      "ratio,p", po::value<string>()->default_value("0.03"), "sampling ratio")(
      "iteration,n", po::value<int>()->default_value(30),
      "iterations per query")("seed,s", po::value<int>()->default_value(0),
                              "random seed");
  po::variables_map vm;
  po::store(po::command_line_parser(argc, argv).options(desc).run(), vm);

  if (vm.count("help") || !vm.count("input") || !vm.count("data")) {
    cout << desc;
    return -1;
  }

  if (!vm.count("query") && !vm.count("build")) {
    cout << "mode is not specified" << endl;
    cout << desc;
    return -1;
  }

  if (vm.count("query") && vm.count("build")) {
    cout << "only one mode can be set" << endl;
    cout << desc;
    return -1;
  }

  string input_str = vm["input"].as<string>();
  string data_str = vm["data"].as<string>();
  double p = stod(vm["ratio"].as<string>());
  int seed = vm["seed"].as<int>();

  string method = vm["method"].as<string>();
  Estimator *estimator = nullptr;
  string summary_str = data_str + string(".") + method;

#ifdef RELATION
  if (method == string("cs")) {
    summary_str = summary_str + ".p" + vm["ratio"].as<string>();
    estimator = new CorrelatedSampling;
  } else if (method == string("bsk")) {
    summary_str = summary_str + ".b" + string(getenv("GCARE_BSK_BUDGET"));
    estimator = new BoundSketch;
    p = std::stod(string(getenv("GCARE_BSK_BUDGET")));
  }
#else
  if (method == string("cset")) {
    summary_str = summary_str + ".p" + vm["ratio"].as<string>();
    estimator = new CharacteristicSets;
  } else if (method == string("impr")) {
    summary_str = summary_str + ".p" + vm["ratio"].as<string>();
    estimator = new Impr;
  } else if (method == string("sumrdf")) {
    summary_str = summary_str + ".p" + vm["ratio"].as<string>();
    estimator = new SumRDF;
  } else if (method == string("wj")) {
    summary_str = summary_str + ".p" + vm["ratio"].as<string>();
    estimator = new WanderJoin;
  } else if (method == string("jsub")) {
    summary_str = summary_str + ".p" + vm["ratio"].as<string>();
    estimator = new JSUB;
  }
#endif
  summary_str = summary_str + ".s" + to_string(seed);

  // std::cout << "summary: " << summary_str << "\n";
  // string output_str = vm["output"].as<string>();

  if (vm.count("build")) {
    // build mode
    DataGraph g;
    if (!g.HasBinary(data_str.c_str())) {
      std::cout << "There is no binary\n";
      g.ReadText(input_str.c_str());
      g.MakeBinary();
      g.WriteBinary(data_str.c_str());
      g.ClearRawData();
    }
    g.ReadBinary(data_str.c_str());
    srand(seed);
    auto chkpt = Clock::now();
    estimator->Summarize(g, summary_str.c_str(), p);
    auto elapsed = chrono::duration<double>(Clock::now() - chkpt);
    double summary_build_time =
        chrono::duration_cast<chrono::milliseconds>(elapsed).count() / 1e3;
    // std::fstream fout;
    // string output_fn = output_str;
    // fout.open(output_fn.c_str(), std::fstream::out);
    // fout << summary_build_time << endl;
    // fout.close();
    cout << summary_build_time << endl;
  } else {
    // query mode
    //  std::cout << "Query Mode\n";
    DataGraph g;
    int chkpt = getValueOfPhysicalMemoryUsage();
    g.ReadBinary(data_str.c_str());
    estimator->ReadSummary(summary_str.c_str());
    int num_iter = vm["iteration"].as<int>();
    // namespace fs = std::filesystem;
    // std::fstream fout;
    // string output_fn = output_str;
    // fout.open(output_fn.c_str(), std::fstream::out);
    // string err_fn = output_str + ".err";
    // std::fstream err_fout;
    // err_fout.open(err_fn.c_str(), std::fstream::out);
    key_t key = ftok("shmfile", 65);
    int shmid = shmget(key, sizeof(QueryResult), 0666 | IPC_CREAT);
    QueryResult *query_result = (QueryResult *)shmat(shmid, (void *)0, 0);
    QueryParams query_params(num_iter, seed, p);
    query(estimator, g, query_params, query_result, input_str.c_str());
    // for (auto &dir_entry :
    //      fs::recursive_directory_iterator(input_str.c_str())) {
    //   if (dir_entry.path().string().find_last_of(".txt") + 1 !=
    //       dir_entry.path().string().length())
    //     continue;
    //   std::cout << "Estimator for " << dir_entry.path().string() << "\n";

    //   // kill
    // }
    shmdt(query_result);
    shmctl(shmid, IPC_RMID, NULL);
    // fout.close();
    // err_fout.close();
  }
  return 0;
}
